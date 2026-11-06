//! The right-click menu: a short list of rows drawn at the pointer, over whatever is under it.
//!
//! [`Menu`] is a view that its owner keeps among its other children. It takes no room (it lays
//! itself out of the flow and draws in a deferred layer), the owner opens it from a right-button
//! press with [`Menu::open`] and a list of [`Item`]s, a label and a callback each. Esc closes it
//! wherever the keyboard is (it never takes the focus: the owner's caret stays where it was) and so
//! does a press outside it. A pick closes the menu first and runs its callback after the current
//! update, so the callback may open the menu again, or [`Menu::pin`] a card in its place: the
//! "open" of a picture shows its preview, which stays until it is dismissed.
//!
//! [`card`] is the rows alone, for a menu that places itself: the sidebar's row menu
//! (`sidebar/ctx.rs`) draws through it and keeps its own anchoring and its name box.

use std::rc::Rc;

use gpui::prelude::FluentBuilder as _;
use gpui::{
    anchored, deferred, div, px, AnyElement, AnyWindowHandle, App, ClickEvent, Context, Div,
    InteractiveElement as _, IntoElement, KeystrokeEvent, MouseDownEvent, ParentElement as _,
    Pixels, Point, Render, SharedString, StatefulInteractiveElement as _, Styled as _,
    Subscription, Window,
};

use super::{menu, menu_item};
use crate::theme::BAD;

/// How wide a menu is.
pub const WIDTH: Pixels = px(140.);

/// What a pick runs: the click that picked the row, and the window and app it happened in. It is
/// what `cx.listener(..)` makes of a closure over the owner's state.
pub type Run = Rc<dyn Fn(&ClickEvent, &mut Window, &mut App)>;

/// One row of a menu.
#[derive(Clone)]
pub struct Item {
    /// What the row says.
    pub label: SharedString,
    /// The row is drawn in the warning colour: picking it throws something away.
    pub danger: bool,
    /// What a click on the row does.
    pub run: Run,
}

impl Item {
    /// A row that runs `run` when it is clicked.
    pub fn new(
        label: impl Into<SharedString>,
        run: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        Item {
            label: label.into(),
            danger: false,
            run: Rc::new(run),
        }
    }

    /// The same row in the warning colour.
    pub fn danger(mut self) -> Self {
        self.danger = true;
        self
    }
}

/// The menu's surface: a [`menu`] of `width` with a row per item, the rows' ids `{prefix}-{index}`.
/// A click on a row ends there: it would otherwise reach whatever the menu hangs from as well.
pub fn card(prefix: &str, width: Pixels, items: impl IntoIterator<Item = Item>) -> Div {
    items
        .into_iter()
        .enumerate()
        .fold(menu().w(width), |card, (ix, item)| {
            let Item { label, danger, run } = item;
            card.child(
                menu_item(SharedString::from(format!("{prefix}-{ix}")))
                    .when(danger, |row| row.text_color(BAD))
                    .child(label)
                    .on_click(move |ev, window, cx| {
                        cx.stop_propagation();
                        run(ev, window, cx);
                    }),
            )
        })
}

/// Draws a pinned card, again on every frame it is shown.
type Build = Rc<dyn Fn(&mut Window, &mut App) -> AnyElement>;

/// What an open menu shows.
enum Body {
    /// Rows to pick from.
    Items(Vec<Item>),
    /// A card that stays until it is dismissed.
    Pinned(Build),
}

/// An open menu: where, and what it shows.
struct Shown {
    at: Point<Pixels>,
    body: Body,
}

/// The right-click menu of a view; see the module's docs.
pub struct Menu {
    shown: Option<Shown>,
    /// The window the menu was last drawn in: the keys of another window are not its business.
    window: Option<AnyWindowHandle>,
    _keys: Subscription,
}

impl Menu {
    /// A closed menu.
    pub fn new(cx: &mut Context<Self>) -> Self {
        // Esc is heard wherever the keyboard is, before the owner's own bindings see it.
        let this = cx.weak_entity();
        let keys = cx.intercept_keystrokes(move |ev, window, cx| {
            this.update(cx, |menu, cx| menu.on_key(ev, window, cx)).ok();
        });
        Menu {
            shown: None,
            window: None,
            _keys: keys,
        }
    }

    /// Opens the menu on `items` with its top left corner at `at` (window coordinates: the pointer
    /// of the right-button press); it moves aside where the window's edge is in the way. Whatever
    /// the menu showed before is replaced.
    pub fn open(&mut self, at: Point<Pixels>, items: Vec<Item>, cx: &mut Context<Self>) {
        self.shown = Some(Shown {
            at,
            body: Body::Items(items),
        });
        cx.notify();
    }

    /// Shows the card `build` draws at `at` instead of rows, until Esc or a press outside it closes
    /// it.
    pub fn pin(
        &mut self,
        at: Point<Pixels>,
        build: impl Fn(&mut Window, &mut App) -> AnyElement + 'static,
        cx: &mut Context<Self>,
    ) {
        self.shown = Some(Shown {
            at,
            body: Body::Pinned(Rc::new(build)),
        });
        cx.notify();
    }

    /// Closes the menu (or the card it pins); false when nothing was showing.
    pub fn close(&mut self, cx: &mut Context<Self>) -> bool {
        if self.shown.take().is_none() {
            return false;
        }
        cx.notify();
        true
    }

    /// Whether the menu, or a card it pins, is showing.
    pub fn is_open(&self) -> bool {
        self.shown.is_some()
    }

    /// A row was clicked: the menu closes, and the row's callback runs once the current update is
    /// over (it may be the one to open this menu again).
    fn pick(&mut self, ix: usize, ev: &ClickEvent, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Shown {
            body: Body::Items(items),
            ..
        }) = self.shown.take()
        else {
            return;
        };
        cx.notify();
        if let Some(item) = items.get(ix) {
            let (run, ev) = (item.run.clone(), ev.clone());
            window.defer(cx, move |window, cx| run(&ev, window, cx));
        }
    }

    /// Esc closes the menu and goes no further: the composer's Esc would interrupt a working
    /// session, the transcript's would leave a subagent's thread.
    fn on_key(&mut self, ev: &KeystrokeEvent, window: &mut Window, cx: &mut Context<Self>) {
        let k = &ev.keystroke;
        if self.window != Some(window.window_handle())
            || k.key != "escape"
            || k.modifiers.modified()
        {
            return;
        }
        if self.close(cx) {
            cx.stop_propagation();
        }
    }
}

impl Render for Menu {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.window = Some(window.window_handle());
        // Out of the flow, so that it takes no room where its owner put it.
        let root = div().absolute();
        let Some(shown) = &self.shown else {
            return root;
        };
        let body = match &shown.body {
            Body::Items(items) => {
                let rows = items.iter().enumerate().map(|(ix, item)| Item {
                    danger: item.danger,
                    ..Item::new(
                        item.label.clone(),
                        cx.listener(move |this, ev: &ClickEvent, window, cx| {
                            this.pick(ix, ev, window, cx)
                        }),
                    )
                });
                card("ctx", WIDTH, rows).into_any_element()
            }
            Body::Pinned(build) => build(window, cx),
        };
        root.child(deferred(
            anchored().position(shown.at).child(
                div()
                    .id("ctx-menu")
                    // Nothing under the menu reacts to the pointer (no hover, no click-through).
                    .occlude()
                    .on_mouse_down_out(cx.listener(|this, _: &MouseDownEvent, _, cx| {
                        this.close(cx);
                    }))
                    .child(body),
            ),
        ))
    }
}
