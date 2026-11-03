//! The terminals: one [`TerminalPane`] per terminal pane of the live sessions, attached as the
//! model lists them. The panel renders the right sidebar's "terminal" tab (a tab per pane, `+`
//! for a shell, `close`) and hands the centre the first terminal of a shell session, or of an
//! agent in its TUI, through [`TerminalPanel::main_pane`]. Owned by the terminal agent
//! (docs/desktop-ui.md §terminal).
//!
//! **Which panes.** A pane is attached when the model lists it as a terminal of a live session:
//! the first [`EAGER`] of them at once (oldest session first), the others when their session is
//! shown, since every attached terminal is a stream the daemon keeps feeding. An attached pane
//! is kept while the pane lives and dropped when it goes. A pane nobody draws stops acking, so
//! the holder idles at its window and an attached pane that is out of sight costs almost
//! nothing. A headless agent's pane 1 stands for its transcript, not for a terminal (its holder
//! has no pty): it is never attached or listed.
//!
//! **One pane, one place.** gpui would draw an entity wherever it is put, but a terminal asks
//! its holder for the size of the box it is drawn in: two boxes would resize it back and forth
//! every frame. So the centre owns a session's first terminal while it shows it (a shell, or an
//! agent in its TUI), and the tab strip leaves it out ([`TerminalPanel::set_centre`]; until told,
//! the panel assumes the centre shows what docs/desktop-ui.md lays out).

pub mod tabs;

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use femme_proto::rpc::{Req, Resp};
use femme_proto::state::{Agent, Mode};
use femme_proto::{PaneId, Sid};
use gpui::prelude::FluentBuilder as _;
use gpui::{
    div, px, App, AppContext as _, AsyncWindowContext, Context, Div, Entity, FocusHandle,
    Focusable, Global, InteractiveElement as _, IntoElement, KeyBinding, ParentElement as _,
    Render, ScrollHandle, Stateful, StatefulInteractiveElement as _, Styled as _, Subscription,
    WeakEntity, Window,
};

use crate::net::Net;
use crate::terminal::pane::TerminalPane;
use crate::theme::{FONT_MONO, META_SIZE, MUTED, PANEL, PAPER};
use crate::ui::{button, button_dark, ruled};

use tabs::{close_req, main_of, on_show, short, tabs_of, terminals, Centre, Shown};

/// Terminals attached as soon as the model lists them, oldest session first. The others attach
/// when their session is shown.
const EAGER: usize = 8;

/// The key context of the panel: its chords apply while the keyboard is inside it.
const CONTEXT: &str = "TerminalPanel";

/// How long, and how often, the panel looks for a pane the daemon has just been asked for, to
/// hand it the keyboard: the daemon answers a moment before it lists the pane.
const FOCUS_TRIES: usize = 60;
const FOCUS_POLL: Duration = Duration::from_millis(50);

#[allow(missing_docs)]
mod actions_def {
    gpui::actions!(terminal_panel, [NextTab, PrevTab]);
}
pub use actions_def::{NextTab, PrevTab};

/// Marks that the panel's chords are in the keymap.
struct KeysBound;

impl Global for KeysBound {}

/// Binds `ctrl-tab` and `ctrl-shift-tab` (next and previous tab) where the keyboard is inside a
/// [`TerminalPanel`]. [`TerminalPanel::new`] does it by itself, once; call it earlier to have the
/// chords in the keymap before a panel exists.
pub fn bind_keys(cx: &mut App) {
    if cx.has_global::<KeysBound>() {
        return;
    }
    cx.set_global(KeysBound);
    cx.bind_keys([
        KeyBinding::new("ctrl-tab", NextTab, Some(CONTEXT)),
        KeyBinding::new("ctrl-shift-tab", PrevTab, Some(CONTEXT)),
    ]);
}

/// A pane the panel holds, with what its tab last showed of it.
struct Held {
    pane: Entity<TerminalPane>,
    /// The title the program gave its terminal: the tab is redrawn when it changes.
    title: Option<String>,
    _watch: Subscription,
}

/// What a request that makes a pane hands the keyboard to once the daemon lists the pane.
#[derive(Clone, Copy, Debug)]
enum Target {
    /// This pane.
    Pane(PaneId),
    /// The first terminal of this session.
    Main(Sid),
}

/// One tab of the strip, as drawn.
struct Tab {
    id: PaneId,
    label: String,
}

/// Everything `render` needs, taken in one look at the model.
struct Snap {
    tabs: Vec<Tab>,
    on_show: Option<usize>,
    pane: Option<Entity<TerminalPane>>,
    can_close: bool,
    note: &'static str,
}

/// The terminals of the live sessions (see the module docs) and the right panel's terminal tab.
pub struct TerminalPanel {
    net: Option<Entity<Net>>,
    /// The session whose panes the tab strip lists.
    session: Option<Sid>,
    centre: Centre,
    /// The attached terminals.
    held: BTreeMap<PaneId, Held>,
    /// The first terminal of each live session that has one (see [`main_of`]), as of the last sync.
    mains: BTreeMap<Sid, PaneId>,
    /// The tab each session shows.
    shown: BTreeMap<Sid, Shown>,
    /// The panel's own focus node: the keyboard is "in the panel" when it is on a pane below it,
    /// and focusing it hands the keyboard to the tab on show.
    focus: FocusHandle,
    strip: ScrollHandle,
    /// The tab the strip was last scrolled to.
    scrolled: Option<PaneId>,
    /// A `+` is waiting for the daemon: another click would open a second pane with the same number.
    opening: bool,
    watching: bool,
    _subs: Vec<Subscription>,
}

impl TerminalPanel {
    /// A panel on `net` (`None`: no daemon, nothing to attach). It takes in what the model lists
    /// now and follows it from here on.
    pub fn new(net: Option<Entity<Net>>, cx: &mut Context<Self>) -> Self {
        bind_keys(cx);
        let mut subs = Vec::new();
        if let Some(n) = &net {
            subs.push(cx.observe(n, |t, _, cx| t.sync(cx)));
        }
        let mut panel = TerminalPanel {
            net,
            session: None,
            centre: Centre::Auto,
            held: BTreeMap::new(),
            mains: BTreeMap::new(),
            shown: BTreeMap::new(),
            focus: cx.focus_handle(),
            strip: ScrollHandle::new(),
            scrolled: None,
            opening: false,
            watching: false,
            _subs: subs,
        };
        panel.sync(cx);
        panel
    }

    /// The session whose panes the tab strip lists. Its terminals are attached now, whatever
    /// came before.
    pub fn set_session(&mut self, sid: Option<Sid>, cx: &mut Context<Self>) {
        self.session = sid;
        self.sync(cx);
    }

    /// The pane the centre draws (`None`: no terminal there, as for a transcript). The tab strip
    /// leaves it out, because one terminal cannot be drawn in two boxes. Until this is called the
    /// panel assumes the centre draws the first terminal of the current session when that is a
    /// shell session or an agent in its TUI (what the shell shows there), and nothing otherwise.
    pub fn set_centre(&mut self, pane: Option<PaneId>, cx: &mut Context<Self>) {
        self.centre = Centre::Is(pane);
        cx.notify();
    }

    /// The attached terminal for `id`, if it is attached.
    pub fn pane(&self, id: PaneId) -> Option<Entity<TerminalPane>> {
        self.held.get(&id).map(|h| h.pane.clone())
    }

    /// The first (lowest numbered) pane of a session, when it is a terminal: what the centre shows
    /// for a shell session or an agent in its TUI. `None` until the daemon lists the pane, for a
    /// headless agent (its first pane is the transcript's) and, past [`EAGER`] terminals, for a
    /// session that has not been [`set_session`](TerminalPanel::set_session) yet.
    pub fn main_pane(&self, sid: Sid) -> Option<Entity<TerminalPane>> {
        self.main_id(sid).and_then(|id| self.pane(id))
    }

    /// The id of [`TerminalPanel::main_pane`] (listed by the daemon, attached or not), for
    /// [`TerminalPanel::set_centre`].
    pub fn main_id(&self, sid: Sid) -> Option<PaneId> {
        self.mains.get(&sid).copied()
    }

    /// The terminal the tab strip has on show, if it has one.
    pub fn shown_pane(&self, cx: &App) -> Option<Entity<TerminalPane>> {
        let (tabs, ix) = self.look(cx);
        self.pane(tabs[ix?])
    }

    /// Follows the window: the daemon is told when the window goes out of sight (it pings less
    /// often), and focusing the panel's handle hands the keyboard to the tab on show. Done at the
    /// first draw anyway; call it earlier to have the first part before the panel is ever drawn.
    pub fn watch_window(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if std::mem::replace(&mut self.watching, true) {
            return;
        }
        self.set_background(window, cx);
        self._subs.push(
            cx.observe_window_activation(window, |t, window, cx| t.set_background(window, cx)),
        );
        let focus = self.focus.clone();
        self._subs
            .push(cx.on_focus(&focus, window, |t, window, cx| t.focus_shown(window, cx)));
    }

    fn set_background(&self, window: &Window, cx: &App) {
        if let Some(net) = &self.net {
            net.read(cx)
                .client()
                .set_background(!window.is_window_active());
        }
    }

    /// Brings the held terminals in line with the model: one for each terminal that appeared
    /// (past [`EAGER`] only for the current session's), none for those that went.
    fn sync(&mut self, cx: &mut Context<Self>) {
        let Some(net) = self.net.clone() else { return };
        let (all, mains, client, rt) = {
            let n = net.read(cx);
            let all = terminals(&n.model);
            let sids: BTreeSet<Sid> = all.iter().map(|p| p.sid).collect();
            let mains = sids
                .into_iter()
                .filter_map(|sid| Some((sid, main_of(&n.model, sid)?)))
                .collect();
            (all, mains, n.client(), n.rt())
        };
        self.mains = mains;
        self.held.retain(|p, _| all.contains(p));
        self.shown
            .retain(|sid, _| all.iter().any(|p| p.sid == *sid));
        for (i, p) in all.iter().enumerate() {
            if (i < EAGER || Some(p.sid) == self.session) && !self.held.contains_key(p) {
                let id = *p;
                let pane = cx.new(|cx| TerminalPane::attach(client.clone(), id, &rt, cx));
                let watch = cx.observe(&pane, move |t, pane, cx| t.pane_changed(id, &pane, cx));
                let title = pane.read(cx).title();
                self.held.insert(
                    id,
                    Held {
                        pane,
                        title,
                        _watch: watch,
                    },
                );
            }
        }
        cx.notify();
    }

    /// A terminal drew or got a frame: its tab is redrawn when the title the program gave it
    /// changed.
    fn pane_changed(&mut self, id: PaneId, pane: &Entity<TerminalPane>, cx: &mut Context<Self>) {
        let title = pane.read(cx).title();
        if let Some(h) = self.held.get_mut(&id).filter(|h| h.title != title) {
            h.title = title;
            cx.notify();
        }
    }

    /// The tabs of the current session and the one on show.
    fn look(&self, cx: &App) -> (Vec<PaneId>, Option<usize>) {
        let Some(net) = &self.net else {
            return (Vec::new(), None);
        };
        let model = &net.read(cx).model;
        let centre = self.centre.pane(model, self.session);
        let tabs = tabs_of(model, self.session, centre);
        let picked = self.session.and_then(|sid| self.shown.get(&sid).copied());
        let ix = on_show(&tabs, picked);
        (tabs, ix)
    }

    /// Takes one look at the model for `render`; the tab on show remembers where it sits.
    fn snap(&mut self, cx: &App) -> Snap {
        let (tabs, ix) = self.look(cx);
        let Some(net) = &self.net else {
            return Snap {
                tabs: Vec::new(),
                on_show: None,
                pane: None,
                can_close: false,
                note: "no daemon: there are no terminals to show",
            };
        };
        let model = &net.read(cx).model;
        if let (Some(sid), Some(i)) = (self.session, ix) {
            if let Some(s) = self.shown.get_mut(&sid).filter(|s| s.pane == tabs[i]) {
                s.ix = i;
            }
        }
        let labelled = tabs
            .iter()
            .map(|p| Tab {
                id: *p,
                label: short(&model.tab_label(*p, self.held.get(p).and_then(|h| h.title.clone()))),
            })
            .collect();
        let note = match self.session {
            None => "no session yet: + starts a shell in the home folder",
            Some(sid) if self.centre.pane(model, Some(sid)).is_some() => {
                "this session's terminal is the one in the middle: + opens another shell in its folder"
            }
            Some(_) => "no terminal in this session: + opens a shell in its folder",
        };
        Snap {
            on_show: ix,
            pane: ix.and_then(|i| self.held.get(&tabs[i]).map(|h| h.pane.clone())),
            can_close: ix.is_some_and(|i| close_req(model, tabs[i]).is_some()),
            tabs: labelled,
            note,
        }
    }

    /// Shows tab `pane` and hands it the keyboard.
    fn select(&mut self, pane: PaneId, window: &mut Window, cx: &mut Context<Self>) {
        let (tabs, _) = self.look(cx);
        let ix = tabs.iter().position(|p| *p == pane).unwrap_or(usize::MAX);
        self.shown.insert(pane.sid, Shown { pane, ix });
        if let Some(h) = self.held.get(&pane) {
            window.focus(&h.pane.focus_handle(cx), cx);
        }
        cx.notify();
    }

    /// Next (`by` 1) or previous (`by` -1) tab, wrapping round.
    fn cycle(&mut self, by: isize, window: &mut Window, cx: &mut Context<Self>) {
        let (tabs, ix) = self.look(cx);
        let Some(ix) = ix.filter(|_| tabs.len() > 1) else {
            return;
        };
        let next = (ix as isize + by).rem_euclid(tabs.len() as isize) as usize;
        self.select(tabs[next], window, cx);
    }

    /// Hands the keyboard to the tab on show, if there is one.
    fn focus_shown(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(pane) = self.shown_pane(cx) {
            window.focus(&pane.focus_handle(cx), cx);
        }
    }

    /// The terminal a [`Target`] names, if it is attached.
    fn target(&self, t: Target) -> Option<Entity<TerminalPane>> {
        match t {
            Target::Pane(p) => self.pane(p),
            Target::Main(sid) => self.main_pane(sid),
        }
    }

    /// "+": a shell in the current session's folder, shown and given the keyboard; with no session
    /// yet, a shell session in the home folder, which becomes the current one.
    fn open_pane(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(net) = self.net.clone() else { return };
        if std::mem::replace(&mut self.opening, true) {
            return;
        }
        let req = match self.session {
            Some(sid) => Req::PaneOpen { sid },
            None => Req::SessionCreate {
                cwd: String::new(),
                agent: Agent::Shell,
                mode: Mode::Tui,
                label: None,
                name: None,
                prompt: None,
            },
        };
        let sent = net.update(cx, |n, cx| n.run(req, cx));
        cx.spawn_in(window, async move |this, cx| {
            let answer = sent.await;
            let target = this
                .update_in(cx, |t, _, cx| {
                    t.opening = false;
                    let target = match answer {
                        // Shown as soon as the daemon lists it: until then the last tab stands in.
                        Some(Resp::Pane(id)) => {
                            t.shown.insert(
                                id.sid,
                                Shown {
                                    pane: id,
                                    ix: usize::MAX,
                                },
                            );
                            Some(Target::Pane(id))
                        }
                        Some(Resp::Session(sid)) => {
                            t.session = Some(sid);
                            net.update(cx, |n, cx| n.set_current(Some(sid), cx));
                            Some(Target::Main(sid))
                        }
                        _ => None,
                    };
                    t.sync(cx);
                    target
                })
                .ok()
                .flatten();
            if let Some(target) = target {
                Self::focus_when_attached(&this, cx, move |t| t.target(target)).await;
            }
        })
        .detach();
        cx.notify();
    }

    /// "close": the tab on show goes (with its session when it is a shell session's own pane),
    /// and the tab that takes its place gets the keyboard. Never an agent's own pane.
    fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(net) = self.net.clone() else { return };
        let (tabs, ix) = self.look(cx);
        let Some(pane) = ix.map(|i| tabs[i]) else {
            return;
        };
        let Some(req) = close_req(&net.read(cx).model, pane) else {
            return;
        };
        let sent = net.update(cx, |n, cx| n.run(req, cx));
        // A refused close leaves the tab where it is: nothing to follow.
        cx.spawn_in(window, async move |this, cx| {
            if sent.await.is_none() {
                return;
            }
            for _ in 0..FOCUS_TRIES {
                let gone = this
                    .update_in(cx, |t, window, cx| {
                        let gone = !t.held.contains_key(&pane);
                        if gone {
                            t.focus_shown(window, cx);
                        }
                        gone
                    })
                    .unwrap_or(true);
                if gone {
                    break;
                }
                cx.background_executor().timer(FOCUS_POLL).await;
            }
        })
        .detach();
    }

    /// Gives the keyboard to the terminal `pick` finds, as soon as there is one: the daemon
    /// answers a request that makes a pane a moment before it lists the pane.
    async fn focus_when_attached(
        this: &WeakEntity<Self>,
        cx: &mut AsyncWindowContext,
        pick: impl Fn(&Self) -> Option<Entity<TerminalPane>>,
    ) {
        for _ in 0..FOCUS_TRIES {
            let done = this
                .update_in(cx, |t, window, cx| match pick(t) {
                    Some(pane) => {
                        window.focus(&pane.focus_handle(cx), cx);
                        true
                    }
                    None => false,
                })
                .unwrap_or(true);
            if done {
                break;
            }
            cx.background_executor().timer(FOCUS_POLL).await;
        }
    }

    /// One tab of the strip: the one on show is the dark button, as the app's other selected tabs.
    fn tab(&self, ix: usize, t: &Tab, on: bool, cx: &mut Context<Self>) -> Stateful<Div> {
        let id = t.id;
        let label = t.label.clone();
        let tab = if on {
            button_dark(("term-tab", ix), label)
        } else {
            button(("term-tab", ix), label)
        };
        tab.h(px(28.))
            .on_click(cx.listener(move |this, _, window, cx| this.select(id, window, cx)))
    }
}

impl Focusable for TerminalPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for TerminalPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.watch_window(window, cx);
        // The keyboard on the panel itself belongs to the tab on show. The shell focuses the panel
        // to go into it, maybe before it was ever drawn, when `on_focus` was not listening yet.
        if self.focus.is_focused(window) {
            cx.defer_in(window, |t, window, cx| t.focus_shown(window, cx));
        }
        let snap = self.snap(cx);
        if let Some(i) = snap.on_show {
            let shown = snap.tabs[i].id;
            if self.scrolled != Some(shown) {
                self.scrolled = Some(shown);
                self.strip.scroll_to_item(i);
            }
        }
        let live = self.net.is_some();
        let tabs: Vec<_> = snap
            .tabs
            .iter()
            .enumerate()
            .map(|(i, t)| self.tab(i, t, snap.on_show == Some(i), cx))
            .collect();
        let band = div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(3.))
            .p(px(3.))
            .bg(PANEL)
            .child(
                div()
                    .id("term-tabs")
                    .flex()
                    .flex_1()
                    .min_w_0()
                    .gap(px(3.))
                    .overflow_x_scroll()
                    .track_scroll(&self.strip)
                    .children(tabs),
            )
            .child(
                button("term-new", "+")
                    .h(px(28.))
                    .on_click(cx.listener(|this, _, window, cx| this.open_pane(window, cx))),
            )
            .when(snap.can_close, |d| {
                d.child(
                    button("term-close", "close")
                        .h(px(28.))
                        .on_click(cx.listener(|this, _, window, cx| this.close(window, cx))),
                )
            });
        let body = div().flex_1().min_h_0();
        let body = match snap.pane {
            Some(pane) => body.child(ruled(
                div()
                    .size_full()
                    .bg(PAPER)
                    .pl(px(8.))
                    .pr(px(4.))
                    .py(px(6.))
                    .child(pane),
            )),
            None => body.p(px(12.)).child(
                div()
                    .font_family(FONT_MONO)
                    .text_size(META_SIZE)
                    .text_color(MUTED)
                    .child(snap.note),
            ),
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .key_context(CONTEXT)
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &NextTab, window, cx| this.cycle(1, window, cx)))
            .on_action(cx.listener(|this, _: &PrevTab, window, cx| this.cycle(-1, window, cx)))
            .when(live, |d| d.child(band))
            .child(body)
    }
}
