//! The `femme` button at the top left and the dropdown it opens: version, check for updates,
//! accounts, reconnect, quit. The shell mounts it (see docs/desktop-ui.md) and handles
//! [`MenuEvent::Accounts`], [`MenuEvent::Reconnect`] and [`MenuEvent::Quit`].
//!
//! - The wordmark is a button: ink border, hard shadow, a caret, a pink fill on hover, pushed in
//!   while pressed or open.
//! - Hovering it for [`OPEN_AFTER`] opens the menu; leaving the button and the menu for
//!   [`CLOSE_AFTER`] closes it. A click opens it and pins it: it stays until Esc, a click outside or
//!   an item. While it is open Up/Down move the highlight, Enter picks the highlighted row and Esc
//!   closes; the menu listens to the keyboard wherever it is, so the focus never moves (the composer
//!   keeps its caret) and a hover-opened menu can be driven too.
//! - "Check for updates" asks the daemon which commit the box runs, compares it with this build's
//!   ([`COMMIT`], stamped by `build.rs`) and shows the [`Verdict`] in a row under the item for
//!   [`RESULT_FOR`]; the comparison itself is pure ([`judge`], [`compare`]) and tested. Behind, it
//!   starts the PC's updater ([`start_updater`]); the shell runs the same check on every connection.

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use femme_proto::rpc::{Req, Resp};
use gpui::prelude::FluentBuilder as _;
use gpui::{
    canvas, deferred, div, point, px, relative, AnyWindowHandle, Bounds, BoxShadow, Context, Div,
    Entity, EventEmitter, FutureExt as _, HoverListenerMode, InteractiveElement as _, IntoElement,
    KeystrokeEvent, MouseButton, MouseDownEvent, ParentElement as _, Pixels, Render, Rgba,
    Stateful, StatefulInteractiveElement as _, Styled as _, Subscription, Task, Window,
};

use crate::motion;
use crate::net::{Link, Net};
use crate::theme::{
    self, BAD, BODY_SIZE, DONE, FONT_BRAND, FONT_MONO, FONT_UI, HOVER, INK, NEON, PANEL, PAPER,
    PAUSE, RULE, WORK,
};
use crate::ui::{dot, ruled, tb};

/// This build's commit: a short sha, or `dev` when `build.rs` had neither `FEMME_COMMIT` nor git.
pub const COMMIT: &str = env!("FEMME_COMMIT");
/// This build's version, from `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// How long the pointer rests on the button before the menu opens (a fly-over does not open it).
pub const OPEN_AFTER: Duration = Duration::from_millis(120);
/// How long the pointer may be off both the button and the menu before a hover-opened menu closes.
pub const CLOSE_AFTER: Duration = Duration::from_millis(300);
/// How long the result of "check for updates" stays under its item.
pub const RESULT_FOR: Duration = Duration::from_secs(10);
/// How long "check for updates" waits for the box before it gives up.
const ASK_FOR: Duration = Duration::from_secs(8);

/// Height of the button: the top bar is 36 px.
const BUTTON_H: Pixels = px(30.);
/// Width of the dropdown.
const CARD_W: Pixels = px(300.);
/// Height of an item row.
const ROW_H: Pixels = px(30.);
/// Wordmark size: the old nav's 32 px in a bar that is 36 px tall instead of 52.
const MARK_SIZE: Pixels = px(26.);
/// How far the wordmark is lifted, in px. Kaushan Script's `f` is 27.5 px from hook to tail at that
/// size and the button is 27.6 px inside its border, but centring the line box leaves the tail 3 px
/// below the button.
const MARK_LIFT: f32 = 3.;

/// What the menu asks the shell to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuEvent {
    /// Open the accounts overlay.
    Accounts,
    /// A check for updates started. The menu runs it and shows the answer itself: the shell has
    /// nothing to do.
    CheckUpdates,
    /// Drop the connection and connect again.
    Reconnect,
    /// Close the app.
    Quit,
}

/// What a check for updates found out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// The box runs the commit this build was made from.
    Current {
        /// This build's commit.
        commit: String,
    },
    /// The box runs another commit: the updater on the PC is started to build it (else its timer
    /// does, within two minutes).
    Behind {
        /// The commit the box runs.
        deployed: String,
    },
    /// This build has no commit stamp, so there is nothing to compare with.
    Unstamped {
        /// The commit the box runs.
        deployed: String,
    },
    /// The box has no `deployed` marker.
    NoMarker,
    /// The daemon does not know `ServerInfo` yet.
    DaemonTooOld,
    /// The request failed, or there was nobody to ask.
    Failed(String),
}

/// `sha` cut to the seven characters that name a commit in a sentence.
pub fn short(sha: &str) -> &str {
    match sha.char_indices().nth(7) {
        Some((end, _)) => &sha[..end],
        None => sha,
    }
}

/// Whether `running` (this build's short sha) and `deployed` (the box's, usually the full one)
/// name the same commit: one is a prefix of the other.
fn same_commit(running: &str, deployed: &str) -> bool {
    let (a, b) = (running.to_ascii_lowercase(), deployed.to_ascii_lowercase());
    !a.is_empty() && !b.is_empty() && (b.starts_with(&a) || a.starts_with(&b))
}

/// Compares this build's commit with what the box says it runs. `running` is a short sha or
/// something that is not one (`dev`: a build made outside git).
pub fn compare(running: &str, deployed: Option<&str>) -> Verdict {
    let Some(deployed) = deployed.map(str::trim).filter(|d| !d.is_empty()) else {
        return Verdict::NoMarker;
    };
    let stamped = !running.is_empty() && running.bytes().all(|b| b.is_ascii_hexdigit());
    if !stamped {
        Verdict::Unstamped {
            deployed: deployed.to_owned(),
        }
    } else if same_commit(running, deployed) {
        Verdict::Current {
            commit: running.to_owned(),
        }
    } else {
        Verdict::Behind {
            deployed: deployed.to_owned(),
        }
    }
}

/// The verdict for the daemon's answer to `Req::ServerInfo`: `answer` is what [`Net::run`] gave,
/// `error` the reason it keeps in `Net::error` when the answer is `None`.
pub fn judge(running: &str, answer: Option<Resp>, error: Option<&str>) -> Verdict {
    match answer {
        Some(Resp::ServerInfo { deployed, .. }) => compare(running, deployed.as_deref()),
        Some(_) => Verdict::Failed("unexpected answer from the daemon".into()),
        // The client words a refused request as `<ErrCode>: <message>`.
        None => match error {
            Some(e) if e.starts_with("Unsupported") => Verdict::DaemonTooOld,
            Some(e) => Verdict::Failed(e.to_owned()),
            None => Verdict::Failed("no answer".into()),
        },
    }
}

impl Verdict {
    /// The words under the item.
    pub fn text(&self) -> String {
        match self {
            Verdict::Current { commit } => format!("up to date ({})", short(commit)),
            Verdict::Behind { deployed } => format!(
                "box runs {}: building it now (femme-desktop-update), then restart",
                short(deployed)
            ),
            Verdict::Unstamped { deployed } => format!(
                "this build has no commit stamp; box runs {}",
                short(deployed)
            ),
            Verdict::NoMarker => "box has no deployed marker".into(),
            Verdict::DaemonTooOld => "daemon too old".into(),
            Verdict::Failed(why) => why.clone(),
        }
    }

    /// The colour of the marker in front of the words: green current, amber when something is
    /// waiting, grey when there is nothing to compare, red when it failed.
    pub fn tone(&self) -> Rgba {
        match self {
            Verdict::Current { .. } => WORK,
            Verdict::Behind { .. } | Verdict::DaemonTooOld => PAUSE,
            Verdict::Unstamped { .. } | Verdict::NoMarker => DONE,
            Verdict::Failed(_) => BAD,
        }
    }
}

/// The items of the dropdown, after the version row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Item {
    Check,
    Accounts,
    Reconnect,
    Quit,
}

impl Item {
    const ALL: [Item; 4] = [Item::Check, Item::Accounts, Item::Reconnect, Item::Quit];

    fn label(self) -> &'static str {
        match self {
            Item::Check => "Check for updates",
            Item::Accounts => "Accounts",
            Item::Reconnect => "Reconnect",
            Item::Quit => "Quit",
        }
    }

    fn id(self) -> &'static str {
        match self {
            Item::Check => "menu-check",
            Item::Accounts => "menu-accounts",
            Item::Reconnect => "menu-reconnect",
            Item::Quit => "menu-quit",
        }
    }

    /// The row after `cur` going down (or up), wrapping round; with nothing selected the first
    /// (last) row.
    fn step(cur: Option<Item>, down: bool) -> Item {
        let n = Item::ALL.len();
        let at = cur.and_then(|c| Item::ALL.iter().position(|i| *i == c));
        let next = match (at, down) {
            (None, true) => 0,
            (None, false) => n - 1,
            (Some(i), true) => (i + 1) % n,
            (Some(i), false) => (i + n - 1) % n,
        };
        Item::ALL[next]
    }
}

/// The row under "Check for updates".
#[derive(Debug)]
enum Phase {
    /// The question is out.
    Asking,
    /// The answer, shown until [`RESULT_FOR`] is over.
    Done(Verdict),
}

/// The hard shadow of [`theme::hard_shadow`], `d` px instead of 3.
fn shadow(d: f32) -> Vec<BoxShadow> {
    let mut s = theme::hard_shadow();
    for b in &mut s {
        b.offset = point(px(d), px(d));
    }
    s
}

/// The `femme` button and its dropdown.
pub struct FemmeMenu {
    net: Option<Entity<Net>>,
    /// The dropdown is showing.
    open: bool,
    /// A click opened it (or picked an item): it stays until Esc, a click outside or an item.
    pinned: bool,
    over_button: bool,
    over_card: bool,
    /// The left button is down on the wordmark.
    pressed: bool,
    /// The row that is lit: under the pointer or reached with the arrows.
    lit: Option<Item>,
    /// `lit` was set with the arrows: only then does Enter pick it (a pointer resting on "Quit"
    /// must not make the Enter that sends a message quit the app).
    lit_by_key: bool,
    check: Option<Phase>,
    /// Where the wordmark was last drawn, for telling a click on it from a click outside.
    button: Rc<Cell<Bounds<Pixels>>>,
    /// The window this menu is drawn in: keys of other windows are not its business.
    window: Option<AnyWindowHandle>,
    /// The pending open or close after a hover change; replacing it cancels it.
    timer: Option<Task<()>>,
    /// The running check.
    asking: Option<Task<()>>,
    /// The wait that clears the result row.
    fading: Option<Task<()>>,
    _keys: Subscription,
}

impl FemmeMenu {
    /// A closed menu; `net` is the daemon's client, `None` on a synthetic run.
    pub fn new(net: Option<Entity<Net>>, cx: &mut Context<Self>) -> Self {
        // While it is open the menu hears the arrows, Enter and Esc wherever the keyboard is: a menu
        // that only hover opened can be driven too, and the composer keeps its caret.
        let this = cx.weak_entity();
        let keys = cx.intercept_keystrokes(move |ev, window, cx| {
            this.update(cx, |t, cx| t.on_key(ev, window, cx)).ok();
        });
        FemmeMenu {
            net,
            open: false,
            pinned: false,
            over_button: false,
            over_card: false,
            pressed: false,
            lit: None,
            lit_by_key: false,
            check: None,
            button: Rc::default(),
            window: None,
            timer: None,
            asking: None,
            fading: None,
            _keys: keys,
        }
    }

    /// Whether the dropdown is showing.
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Opens the dropdown pinned, as a click on the wordmark does (for a shortcut of the shell's).
    pub fn open(&mut self, cx: &mut Context<Self>) {
        self.timer = None;
        if !self.open {
            self.open = true;
            self.lit = None;
            self.lit_by_key = false;
        }
        self.pinned = true;
        cx.notify();
    }

    /// Closes the dropdown.
    pub fn close(&mut self, cx: &mut Context<Self>) {
        self.open = false;
        self.pinned = false;
        self.over_card = false;
        self.lit = None;
        self.lit_by_key = false;
        self.timer = None;
        cx.notify();
    }

    /// The pointer moved on or off the button or the menu: arm the open or close that follows.
    fn settle(&mut self, cx: &mut Context<Self>) {
        self.timer = None;
        if self.open && !self.pinned && !self.over_button && !self.over_card {
            self.timer = Some(self.after(CLOSE_AFTER, cx, |t, cx| {
                if !t.pinned && !t.over_button && !t.over_card {
                    t.close(cx);
                }
            }));
        } else if !self.open && self.over_button {
            self.timer = Some(self.after(OPEN_AFTER, cx, |t, cx| {
                if t.over_button && !t.open {
                    t.open = true;
                    t.lit = None;
                    t.lit_by_key = false;
                    cx.notify();
                }
            }));
        }
    }

    /// Runs `then` on this menu once `delay` has passed; dropping the task cancels it.
    fn after(
        &self,
        delay: Duration,
        cx: &mut Context<Self>,
        then: impl FnOnce(&mut Self, &mut Context<Self>) + 'static,
    ) -> Task<()> {
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            this.update(cx, then).ok();
        })
    }

    /// A press on the wordmark pins the menu (opening it first), or, when it is pinned already,
    /// closes it, like a native menu button.
    fn press(&mut self, cx: &mut Context<Self>) {
        self.pressed = true;
        if self.open && self.pinned {
            self.close(cx);
        } else {
            self.open(cx);
        }
    }

    /// Does what `item` says.
    fn activate(&mut self, item: Item, cx: &mut Context<Self>) {
        let event = match item {
            Item::Check => {
                // The answer shows under the item: stay open for it.
                self.open(cx);
                cx.emit(MenuEvent::CheckUpdates);
                self.check_updates(cx);
                return;
            }
            Item::Accounts => MenuEvent::Accounts,
            Item::Reconnect => MenuEvent::Reconnect,
            Item::Quit => MenuEvent::Quit,
        };
        self.close(cx);
        cx.emit(event);
    }

    /// Asks the box which commit it runs and shows what that means for this build; when the box is
    /// ahead, starts the PC's updater. The shell calls it on every connection too, so a merge to
    /// main (the box deploys and restarts femmed) builds the new app without anyone clicking.
    pub fn check_updates(&mut self, cx: &mut Context<Self>) {
        self.check = Some(Phase::Asking);
        self.fading = None;
        cx.notify();
        let Some(net) = self.net.clone() else {
            return self.finish(
                Verdict::Failed("no daemon: this run is synthetic".into()),
                cx,
            );
        };
        // Off the box there is nobody to ask: say so now instead of waiting for the next connection.
        let (link, status) = {
            let n = net.read(cx);
            (n.model.link.clone(), n.model.status())
        };
        if !matches!(link, Link::Up(_)) {
            return self.finish(Verdict::Failed(status), cx);
        }
        let answer = net.update(cx, |n, cx| n.run(Req::ServerInfo, cx));
        self.asking = Some(cx.spawn(async move |this, cx| {
            let answer = answer.with_timeout(ASK_FOR, cx.background_executor()).await;
            this.update(cx, |t, cx| {
                let error = t.net.as_ref().and_then(|n| n.read(cx).error.clone());
                let verdict = match answer {
                    Ok(answer) => judge(COMMIT, answer, error.as_deref()),
                    Err(_) => Verdict::Failed("the box did not answer".into()),
                };
                if matches!(verdict, Verdict::Behind { .. }) {
                    Self::start_updater();
                }
                t.finish(verdict, cx);
            })
            .ok();
        }));
    }

    /// Starts the PC's updater now (`laptop/femme-desktop-update.sh`, the unit its timer runs) instead
/// of waiting for the timer's next tick. Best effort: no systemd (macOS), no unit (the checkout
/// was not set up with pc-setup.sh), a build already running: nothing happens and the timer still
/// does its work.
fn start_updater() {
    let _ = std::process::Command::new("systemctl")
        .args(["--user", "start", "--no-block", "femme-desktop-update.service"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// Shows `verdict` under the item for [`RESULT_FOR`].
    fn finish(&mut self, verdict: Verdict, cx: &mut Context<Self>) {
        self.check = Some(Phase::Done(verdict));
        cx.notify();
        self.fading = Some(self.after(RESULT_FOR, cx, |t, cx| {
            t.check = None;
            cx.notify();
        }));
    }

    /// Every key down in the app, while the menu is open: Up/Down move the highlight, Enter picks
    /// the row the arrows chose, Esc closes. Anything else goes where it was going.
    fn on_key(&mut self, ev: &KeystrokeEvent, window: &Window, cx: &mut Context<Self>) {
        let k = &ev.keystroke;
        if !self.open
            || self.window != Some(window.window_handle())
            || k.modifiers.control
            || k.modifiers.alt
            || k.modifiers.platform
        {
            return;
        }
        match k.key.as_str() {
            "escape" => self.close(cx),
            key @ ("down" | "up") => {
                self.lit = Some(Item::step(self.lit, key == "down"));
                self.lit_by_key = true;
                cx.notify();
            }
            "enter" => match self.lit.filter(|_| self.lit_by_key) {
                Some(item) => self.activate(item, cx),
                None => return,
            },
            _ => return,
        }
        cx.stop_propagation();
    }

    /// The wordmark: neon Kaushan Script over the 2 px ink copy the board draws as its shadow (gpui
    /// has no text-shadow, so it is drawn twice). The copies must not wrap: the shadow sits 2 px in,
    /// so it has 2 px less room than its text and would move its last letter to a second line.
    fn wordmark(face: Rgba) -> Div {
        let word = |color: Rgba| div().text_color(color).child("femme");
        div()
            .relative()
            .top(px(-MARK_LIFT))
            .whitespace_nowrap()
            .font_family(FONT_BRAND)
            .text_size(MARK_SIZE)
            .line_height(relative(1.1))
            .child(word(INK).absolute().top(px(2.)).left(px(2.)))
            .child(word(face))
    }

    /// The face of the button: paper with a hard shadow, pink under the pointer, pushed onto its
    /// shadow while pressed or while its menu is open.
    fn face(&self) -> Div {
        let down = self.pressed || self.open;
        let pink = self.over_button || self.open;
        let mut face = ruled(
            div()
                .relative()
                .flex()
                .items_center()
                .gap(px(6.))
                .h(BUTTON_H)
                .px(px(10.))
                // Nothing of the wordmark draws outside the button, whatever the rasteriser does.
                .overflow_hidden()
                .bg(if pink { HOVER } else { PAPER }),
        );
        face.style().box_shadow = Some(if down { Vec::new() } else { shadow(2.) });
        let face = if down {
            face.top(px(2.)).left(px(2.))
        } else {
            face
        };
        face.child(Self::wordmark(if pink { PAPER } else { NEON }))
            .child(
                div()
                    .font_family(FONT_MONO)
                    .text_size(px(13.))
                    .text_color(INK)
                    .child("▾"),
            )
    }

    /// One row of the dropdown.
    fn row(&self, item: Item, cx: &mut Context<Self>) -> Stateful<Div> {
        div()
            .id(item.id())
            .flex()
            .items_center()
            .h(ROW_H)
            .px(px(12.))
            .cursor_pointer()
            .bg(if self.lit == Some(item) { HOVER } else { PAPER })
            .on_mouse_move(cx.listener(move |t, _, _, cx| {
                if t.lit != Some(item) || t.lit_by_key {
                    t.lit = Some(item);
                    t.lit_by_key = false;
                    cx.notify();
                }
            }))
            .on_click(cx.listener(move |t, _, _, cx| t.activate(item, cx)))
            .child(item.label())
    }

    /// The row under "Check for updates": a marker and the words, wrapping.
    fn result(&self) -> Option<Div> {
        let (tone, text) = match self.check.as_ref()? {
            Phase::Asking => (DONE, "asking the box…".to_owned()),
            Phase::Done(v) => (v.tone(), v.text()),
        };
        Some(
            div()
                .flex()
                .items_start()
                .gap(px(8.))
                .px(px(12.))
                .py(px(7.))
                .bg(PANEL)
                .child(dot(tone).mt(px(4.)))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_size(px(13.))
                        .line_height(relative(1.3))
                        .child(text),
                ),
        )
    }

    /// The dropdown: the version as a black title bar, then the items.
    fn card(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        let button = self.button.clone();
        let rule = || div().flex_none().h(RULE).bg(INK);
        let mut card = ruled(
            div()
                .id("femme-menu-card")
                .occlude()
                .flex()
                .flex_col()
                .w(CARD_W)
                .bg(PAPER)
                .font_family(FONT_UI)
                .text_size(BODY_SIZE)
                .line_height(relative(1.3))
                .text_color(INK),
        );
        card.style().box_shadow = Some(theme::hard_shadow());
        card.on_hover(cx.listener(|t, over: &bool, _, cx| {
            t.over_card = *over;
            if !*over && !t.lit_by_key {
                t.lit = None;
            }
            t.settle(cx);
            cx.notify();
        }))
        .hover_listener_mode(HoverListenerMode::InputModalityIndependent)
        // The wordmark takes its own presses; anything else outside closes.
        .on_mouse_down_out(cx.listener(move |t, ev: &MouseDownEvent, _, cx| {
            if !button.get().contains(&ev.position) {
                t.close(cx);
            }
        }))
        // A click anywhere in the menu pins it.
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|t, _, _, cx| {
                t.timer = None;
                t.pinned = true;
                cx.notify();
            }),
        )
        .child(tb(format!("femme-desktop {VERSION} · {COMMIT}")))
        .child(self.row(Item::Check, cx))
        .children(self.result())
        .child(rule())
        .child(self.row(Item::Accounts, cx))
        .child(self.row(Item::Reconnect, cx))
        .child(rule())
        .child(self.row(Item::Quit, cx))
    }
}

impl EventEmitter<MenuEvent> for FemmeMenu {}

impl Render for FemmeMenu {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.window = Some(window.window_handle());
        let measured = self.button.clone();
        let card = motion::appear("femme-menu-card", self.open, window, cx);
        div()
            .id("femme-menu")
            .relative()
            .flex_none()
            .cursor_pointer()
            .on_hover(cx.listener(|t, over: &bool, _, cx| {
                t.over_button = *over;
                t.settle(cx);
                cx.notify();
            }))
            // A key press must not count as the pointer leaving: the menu is driven with the arrows
            // while the pointer rests on the button.
            .hover_listener_mode(HoverListenerMode::InputModalityIndependent)
            .on_mouse_down(MouseButton::Left, cx.listener(|t, _, _, cx| t.press(cx)))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|t, _, _, cx| {
                    t.pressed = false;
                    cx.notify();
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|t, _, _, cx| {
                    t.pressed = false;
                    cx.notify();
                }),
            )
            .child(self.face())
            .child(
                canvas(move |bounds, _, _| measured.set(bounds), |_, _, _, _| {})
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full(),
            )
            // The card drops 4 px into place and fades, and goes the same way (drawn while it does).
            .when_some(card, |d, a| {
                d.child(deferred(
                    div()
                        .absolute()
                        .top_full()
                        .mt(px(6.) - a.offset(4.))
                        .left_0()
                        .opacity(a.opacity())
                        .child(self.card(cx)),
                ))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: &str = "8b23d2d0f5c3a1e94b7d6a2c10e8f9b3a4d5c6e7";

    fn info(deployed: Option<&str>) -> Option<Resp> {
        Some(Resp::ServerInfo {
            version: "0.1.0".into(),
            deployed: deployed.map(str::to_owned),
        })
    }

    #[test]
    fn the_same_commit_is_up_to_date() {
        assert_eq!(
            compare("8b23d2d", Some("8b23d2d")),
            Verdict::Current {
                commit: "8b23d2d".into()
            }
        );
    }

    #[test]
    fn a_short_sha_matches_the_full_one_by_prefix() {
        let v = compare("8b23d2d", Some(FULL));
        assert_eq!(
            v,
            Verdict::Current {
                commit: "8b23d2d".into()
            }
        );
        assert_eq!(v.text(), "up to date (8b23d2d)");
        // The marker is a file: a trailing newline and capitals change nothing.
        assert!(matches!(
            compare("8B23D2D", Some(&format!("{FULL}\n"))),
            Verdict::Current { .. }
        ));
        // An eight-character stamp (git lengthens short shas when seven are ambiguous).
        assert!(matches!(
            compare("8b23d2d0", Some(FULL)),
            Verdict::Current { .. }
        ));
    }

    #[test]
    fn another_commit_says_the_updater_builds_it_now() {
        let v = compare("8b23d2d", Some("1f2e3d4c5b6a79880011223344556677889900aa"));
        assert_eq!(
            v,
            Verdict::Behind {
                deployed: "1f2e3d4c5b6a79880011223344556677889900aa".into()
            }
        );
        assert_eq!(
            v.text(),
            "box runs 1f2e3d4: building it now (femme-desktop-update), then restart"
        );
        // A different eighth character is a different commit: the prefix is not just seven long.
        assert!(matches!(
            compare("8b23d2d1", Some(FULL)),
            Verdict::Behind { .. }
        ));
    }

    #[test]
    fn a_box_without_a_marker_has_nothing_to_compare() {
        for deployed in [None, Some(""), Some("  \n")] {
            let v = compare("8b23d2d", deployed);
            assert_eq!(v, Verdict::NoMarker);
            assert_eq!(v.text(), "box has no deployed marker");
        }
    }

    #[test]
    fn a_build_without_a_stamp_is_not_called_behind() {
        let v = compare("dev", Some(FULL));
        assert_eq!(v.text(), "this build has no commit stamp; box runs 8b23d2d");
        assert!(matches!(compare("", Some(FULL)), Verdict::Unstamped { .. }));
        // No marker still wins: that is the more useful thing to say.
        assert_eq!(compare("dev", None), Verdict::NoMarker);
    }

    #[test]
    fn an_old_daemon_is_told_apart_from_a_failed_request() {
        let old = judge("8b23d2d", None, Some("Unsupported: unknown request"));
        assert_eq!(old, Verdict::DaemonTooOld);
        assert_eq!(old.text(), "daemon too old");
        let timeout = judge("8b23d2d", None, Some("no answer within the deadline"));
        assert_eq!(timeout.text(), "no answer within the deadline");
        assert_eq!(judge("8b23d2d", None, None).text(), "no answer");
    }

    #[test]
    fn the_answer_is_judged_by_its_marker() {
        assert!(matches!(
            judge("8b23d2d", info(Some(FULL)), None),
            Verdict::Current { .. }
        ));
        assert_eq!(judge("8b23d2d", info(None), None), Verdict::NoMarker);
        // A stale error from an earlier request does not spoil a good answer.
        assert!(matches!(
            judge("8b23d2d", info(Some(FULL)), Some("Unsupported: x")),
            Verdict::Current { .. }
        ));
        assert!(matches!(
            judge("8b23d2d", Some(Resp::Ok), None),
            Verdict::Failed(_)
        ));
    }

    #[test]
    fn short_cuts_at_seven_characters_and_never_inside_one() {
        assert_eq!(short(FULL), "8b23d2d");
        assert_eq!(short("abc"), "abc");
        assert_eq!(short("ééééééééé"), "ééééééé");
    }

    #[test]
    fn the_arrows_wrap_round_the_items() {
        use Item::*;
        assert_eq!(Item::step(None, true), Check);
        assert_eq!(Item::step(None, false), Quit);
        assert_eq!(Item::step(Some(Check), true), Accounts);
        assert_eq!(Item::step(Some(Quit), true), Check);
        assert_eq!(Item::step(Some(Check), false), Quit);
        assert_eq!(Item::step(Some(Reconnect), false), Accounts);
    }

    #[test]
    fn this_build_knows_its_commit() {
        assert!(!COMMIT.is_empty());
        assert!(!VERSION.is_empty());
    }
}
