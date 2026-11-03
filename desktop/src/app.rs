//! The desktop shell: the frame that mounts every module and owns the keyboard.
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────────────┐
//! │ [femme ▾]  quic 23 ms                                                           │  top bar
//! ├────────────┬─────────────────────────────────────────────────────┬──────────────┤
//! │ sessions ‹ │ transcript (agent)  OR  terminal (shell)            │ terminal     │
//! │            │                                                     │ files        │
//! │            ├─────────────────────────────────────────────────────┤ (hover strip │
//! │            │ composer                                            │  opens panel)│
//! └────────────┴─────────────────────────────────────────────────────┴──────────────┘
//! ```
//!
//! The pure rules (which view the centre shows, what the right-hand panel does with the pointer, the
//! focus cycle) live in [`crate::frame`]; this file wires them to the daemon's state and draws.
//!
//! The shell draws the outer paper surface and ink rule of the transcript, the terminal and the
//! right-hand panel; the module inside fills its box and draws no outer border. The sidebar and the
//! composer are the exceptions: they draw their own boxes on the desk (the sidebar sizes itself, and
//! collapses to a strip of dots; the shell follows its width).
//!
//! Frame cost: the sidebar, transcript, terminal panel and files panel are mounted with
//! `.cached(..)`, so a frame in which only a terminal pane changed replays their recorded paint.
//! Only the shell itself (a few dozen divs) and the pane views re-render.

#[cfg(feature = "bench")]
use std::time::Duration;

use femme_client::Event;
use gpui::prelude::FluentBuilder as _;
use gpui::{
    div, px, relative, size, AnyElement, App, AppContext as _, Context, Entity, FocusHandle,
    Focusable, InteractiveElement as _, IntoElement, KeyBinding, ParentElement as _, Render,
    SharedString, StatefulInteractiveElement as _, StyleRefinement, Styled as _, Subscription,
    Task, TitlebarOptions, Window, WindowBounds, WindowOptions,
};
use gpui_component::tooltip::Tooltip;
use gpui_component::Root;

use crate::accounts::AccountsOverlay;
#[cfg(feature = "bench")]
use crate::bench::{Bench, BenchConfig, PaneStats};
use crate::composer::Composer;
use crate::connect::Connect;
use crate::files::FilesPanel;
use crate::frame::{next_stop, short_error, Centre, Right, Shown, Stop};
use crate::menu::{FemmeMenu, MenuEvent};
use crate::net::{ClientEvent, Net, Started};
use crate::session;
use crate::sidebar::{CollapsedChanged, OpenSession, Sidebar};
use crate::terminal::panel::TerminalPanel;
use crate::terminal::{SourceKind, TerminalPane};
use crate::theme::{BODY_SIZE, FONT_PIXEL, FONT_UI, GUTTER, INK, MUTED, SURFACE, TOP_BAR_H};
use crate::transcript::Transcript;
use crate::ui::{self, ruled, DotGrid};

mod right;

use right::CLOSE_AFTER;

#[allow(missing_docs)]
mod actions_def {
    gpui::actions!(
        femme,
        [
            ToggleSidebar,
            ToggleTerminal,
            CloseRight,
            FocusNext,
            FocusPrev
        ]
    );
}
pub use actions_def::*;

/// `cmd-<key>` on macOS, `ctrl-<key>` elsewhere (gpui keystroke strings have no `secondary` alias;
/// `Modifiers::secondary()` exists only at match time). A macro so the result stays a `&'static str`.
#[cfg(target_os = "macos")]
macro_rules! modkey {
    ($k:literal) => {
        concat!("cmd-", $k)
    };
}
#[cfg(not(target_os = "macos"))]
macro_rules! modkey {
    ($k:literal) => {
        concat!("ctrl-", $k)
    };
}

/// Binds the shell's keys: mod+b sidebar, mod+j the terminal tab, mod+k closes the right-hand
/// panel, F6 and shift+F6 walk the focus. (The accounts card hears escape itself.)
pub fn bind_keys(cx: &mut App) {
    // The terminal panel's own chords (ctrl-tab, ctrl-shift-tab), in the keymap before the panel exists.
    crate::terminal::panel::bind_keys(cx);
    cx.bind_keys([
        KeyBinding::new(modkey!("b"), ToggleSidebar, Some("Femme")),
        KeyBinding::new(modkey!("j"), ToggleTerminal, Some("Femme")),
        KeyBinding::new(modkey!("k"), CloseRight, Some("Femme")),
        KeyBinding::new("f6", FocusNext, Some("Femme")),
        KeyBinding::new("shift-f6", FocusPrev, Some("Femme")),
    ]);
}

/// Everything the command line configures. `bench`, `panes`, `hz`, `modes`, `full_height`,
/// `target_ms` and `seed` only drive the benchmark (synthetic terminals, no daemon).
#[derive(Clone, Debug)]
pub struct Config {
    /// Run the benchmark for this many seconds, then print the report and exit.
    pub bench: Option<u64>,
    /// Synthetic terminal panes of the benchmark.
    pub panes: usize,
    /// Synthetic terminal frame rate.
    pub hz: u32,
    /// What the panes show, not empty: pane `i` takes `modes[i % modes.len()]`.
    pub modes: Vec<SourceKind>,
    /// The benchmark's panes take the whole window (no frame around them), so they are as tall as
    /// the window allows.
    pub full_height: bool,
    /// Frame-time target in milliseconds for the benchmark verdict; `None` follows the display.
    pub target_ms: Option<f64>,
    /// Seed of every synthetic source.
    pub seed: u64,
    /// Requested window size in logical pixels (a compositor may override it).
    pub window: (f32, f32),
    /// The daemon to show; `None` shows the frame with "no daemon configured".
    pub connect: Option<Connect>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bench: None,
            panes: 4,
            hz: 60,
            modes: vec![SourceKind::Fire],
            full_height: false,
            target_ms: None,
            seed: 1,
            window: (1440., 900.),
            connect: None,
        }
    }
}

impl Config {
    /// What pane `i` shows.
    pub fn mode(&self, i: usize) -> SourceKind {
        self.modes[i % self.modes.len()]
    }

    /// What every pane shows, comma separated (`fire,fire,cat`).
    pub fn mode_label(&self) -> String {
        (0..self.panes)
            .map(|i| self.mode(i).name())
            .collect::<Vec<_>>()
            .join(",")
    }
}

/// Opens the main window and installs the shell in it. Call from inside `Application::run`.
///
/// The display comes from [`crate::display::place`] (`FEMME_DISPLAY`, built-in by default). A benchmark
/// window is not focused and the app is not activated, so it never takes the keyboard from whoever
/// is working. `started` is the client of the daemon when `cfg.connect` asked for one.
pub fn open(cfg: Config, rt: tokio::runtime::Handle, started: Option<Started>, cx: &mut App) {
    let bench = cfg.bench.is_some();
    let place = crate::display::place(cx, size(px(cfg.window.0), px(cfg.window.1)));
    let opts = WindowOptions {
        display_id: place.display_id,
        window_bounds: Some(WindowBounds::Windowed(place.bounds)),
        focus: !bench,
        titlebar: Some(TitlebarOptions {
            title: Some("femme".into()),
            ..Default::default()
        }),
        app_id: Some("femme-desktop".into()),
        // A benchmark must not be throttled to 30 fps just because the compositor did not focus it.
        inactive_frame_interval: if bench {
            None
        } else {
            WindowOptions::default().inactive_frame_interval
        },
        ..Default::default()
    };
    cx.open_window(opts, |window, cx| {
        let app = cx.new(|cx| FemmeApp::new(cfg, rt, started, window, cx));
        window.focus(&app.read(cx).focus.clone(), cx);
        cx.new(|cx| Root::new(app, window, cx))
    })
    .expect("open the main window");
    if !bench {
        cx.activate(true);
    }
}

/// Waits out the warm-up, measures for `secs` seconds, prints the report and quits.
#[cfg(feature = "bench")]
fn bench_task(
    secs: u64,
    cfg: &Config,
    panes: &[Entity<TerminalPane>],
    window: &Window,
    cx: &mut Context<FemmeApp>,
) -> Task<()> {
    let handle = window.window_handle();
    let panes: Vec<_> = panes.iter().map(Entity::downgrade).collect();
    let mut bc = BenchConfig {
        seconds: secs,
        panes: cfg.panes,
        hz: cfg.hz,
        mode: cfg.mode_label(),
        full_height: cfg.full_height,
        target_ms: cfg.target_ms,
        window: None,
    };
    cx.spawn(async move |_, cx| {
        let stats = |cx: &gpui::AsyncApp| -> Vec<PaneStats> {
            panes
                .iter()
                .filter_map(|p| p.read_with(cx, |p, _| p.stats()).ok())
                .collect()
        };
        cx.background_executor().timer(crate::bench::WARMUP).await;
        bc.window = cx.update(|cx| {
            handle
                .update(cx, |_, window, _| {
                    let s = window.viewport_size();
                    (
                        f32::from(s.width).round() as u32,
                        f32::from(s.height).round() as u32,
                        window.scale_factor(),
                    )
                })
                .ok()
        });
        let before = stats(cx);
        let mut b = Bench::start(bc);
        while b.elapsed() < Duration::from_secs(secs) {
            cx.background_executor()
                .timer(Duration::from_millis(250))
                .await;
            b.poll();
        }
        // Frames applied count from the start of measuring; resyncs are those of the whole run.
        let panes = stats(cx)
            .into_iter()
            .zip(before)
            .map(|(after, before)| PaneStats {
                applied: after.applied - before.applied,
                ..after
            })
            .collect();
        let report = b.finish(panes);
        println!("{report}");
        if report.frames == 0 {
            println!(
                "warning: no frames were presented; is the window visible and on a live display?"
            );
        }
        cx.update(|cx| cx.quit());
    })
}

/// What the app says about having no session: what it is starting, or why that failed.
fn note_of(net: &Net) -> Option<&str> {
    net.starting().or(net.error.as_deref())
}

/// The top bar's line and the whole text behind it: the link's status as [`Net::status_line`] says it,
/// with the failed request it ends in cut to its first clause (the tooltip has it whole).
fn status_of(net: &Net) -> (String, String) {
    let whole = net.status_line();
    let cut = net.error.as_deref().and_then(|error| {
        let head = whole.strip_suffix(&format!(" | {error}"))?;
        Some(format!("{head} | {}", short_error(error)))
    });
    (cut.unwrap_or_else(|| whole.clone()), whole)
}

/// The shell view.
pub struct FemmeApp {
    cfg: Config,
    dots: DotGrid,
    net: Option<Entity<Net>>,
    menu: Entity<FemmeMenu>,
    sidebar: Entity<Sidebar>,
    transcript: Entity<Transcript>,
    composer: Entity<Composer>,
    terminal: Entity<TerminalPanel>,
    files: Entity<FilesPanel>,
    accounts: Entity<AccountsOverlay>,
    /// The benchmark's synthetic terminals; empty otherwise.
    panes: Vec<Entity<TerminalPane>>,
    /// The window's own stop: the key context of the shell is always in the dispatch path.
    focus: FocusHandle,
    centre_focus: FocusHandle,
    right_focus: FocusHandle,
    /// What the centre shows, and what the modules were last told.
    shown: Shown,
    right: Right,
    /// The accounts overlay was open at the last look.
    modal_open: bool,
    /// The centre should take the keyboard as soon as there is something to take it.
    want_focus: bool,
    /// A module's handle took the keyboard: `verify_focus` looks once the frame that carries it is drawn.
    check_focus: bool,
    /// The pending open/close decision of the right-hand panel.
    settle: Option<Task<()>>,
    _subs: Vec<Subscription>,
    _bench: Option<Task<()>>,
}

impl FemmeApp {
    fn new(
        cfg: Config,
        rt: tokio::runtime::Handle,
        started: Option<Started>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let net = started.map(|s| cx.new(|cx| Net::new(s.client, rt.clone(), s.enroll, cx)));
        let menu = cx.new(|cx| FemmeMenu::new(net.clone(), cx));
        let sidebar = cx.new(|cx| Sidebar::new(net.clone(), cx));
        let transcript = cx.new(|cx| Transcript::new(net.clone(), cx));
        let composer = cx.new(|cx| Composer::new(net.clone(), cx));
        let terminal = cx.new(|cx| TerminalPanel::new(net.clone(), cx));
        let files = cx.new(|cx| FilesPanel::new(net.clone(), cx));
        let accounts = cx.new(|cx| AccountsOverlay::new(net.clone(), cx));
        // Synthetic terminals exist for the benchmark only; the app itself has no mock.
        let panes: Vec<Entity<TerminalPane>> = (0..cfg.panes)
            .filter(|_| cfg.bench.is_some())
            .map(|i| {
                // Stagger the sources: independent terminals do not tick in the same instant, and
                // aligned ones would hide half the frames a real session draws.
                let phase = i as f32 / cfg.panes.max(1) as f32;
                cx.new(|cx| {
                    TerminalPane::new(i, cfg.mode(i), cfg.hz, cfg.seed + i as u64, phase, &rt, cx)
                })
            })
            .collect();
        #[cfg(feature = "bench")]
        let bench = cfg
            .bench
            .map(|secs| bench_task(secs, &cfg, &panes, window, cx));
        #[cfg(not(feature = "bench"))]
        let bench = None;

        let right_focus = cx.focus_handle();
        let mut subs = vec![
            cx.subscribe_in(&menu, window, |t, _, ev: &MenuEvent, window, cx| {
                t.on_menu(*ev, window, cx)
            }),
            cx.subscribe_in(&sidebar, window, |t, _, ev: &OpenSession, _, cx| {
                t.open_session(ev.0, cx)
            }),
            // The column's width is the sidebar's to say: collapsing it resizes the frame.
            cx.subscribe(&sidebar, |_, _, _: &CollapsedChanged, cx| cx.notify()),
            cx.observe_in(&accounts, window, |t, _, window, cx| {
                t.accounts_changed(window, cx)
            }),
            cx.observe_in(&terminal, window, |t, _, window, cx| {
                t.terminal_changed(window, cx)
            }),
            // Clicking away from the panel, or F6, leaves it open until the pointer is gone too.
            cx.on_focus_out(&right_focus, window, |t, _, window, cx| {
                t.poke(CLOSE_AFTER, window, cx)
            }),
        ];
        if let Some(net) = &net {
            subs.push(cx.observe_in(net, window, |t, _, window, cx| t.sync(window, cx)));
            subs.push(cx.subscribe(net, |t, net, ev: &ClientEvent, cx| {
                t.on_client_event(&net, ev, cx)
            }));
            // A window nobody looks at needs the daemon's attention less often.
            subs.push(cx.observe_window_activation(window, |t, window, cx| {
                if let Some(net) = &t.net {
                    net.read(cx)
                        .client()
                        .set_background(!window.is_window_active());
                }
            }));
        }
        let shown = net.as_ref().map_or_else(Shown::no_daemon, |n| {
            let net = n.read(cx);
            Shown::of(&net.model, note_of(net))
        });
        Self {
            dots: DotGrid::new(window.scale_factor(), cx),
            focus: cx.focus_handle(),
            centre_focus: cx.focus_handle(),
            right_focus,
            cfg,
            net,
            menu,
            sidebar,
            transcript,
            composer,
            terminal,
            files,
            accounts,
            panes,
            shown,
            right: Right::new(),
            modal_open: false,
            want_focus: false,
            check_focus: false,
            settle: None,
            _subs: subs,
            _bench: bench,
        }
    }

    /// A connection is up, the first or a later one: `open_at_start` picks or creates the session to
    /// show. It decides once, and does nothing more unless a try that found nothing to show or to
    /// start is asked again.
    fn on_client_event(&mut self, net: &Entity<Net>, ev: &ClientEvent, cx: &mut Context<Self>) {
        if matches!(ev.0, Event::Connected { .. }) {
            session::open_at_start(net, cx);
        }
    }

    /// Brings the modules in line with the session the daemon's state says is current, and tells
    /// the keyboard where to go when it changed.
    fn sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let shown = match &self.net {
            Some(net) => {
                let net = net.read(cx);
                self.shown.after(&net.model, note_of(net))
            }
            None => Shown::no_daemon(),
        };
        let mut new_session = false;
        if shown != self.shown {
            let old = std::mem::replace(&mut self.shown, shown);
            let (sid, cwd, transcript) = (
                self.shown.sid,
                self.shown.cwd.clone(),
                self.shown.transcript(),
            );
            if old.sid != sid {
                self.sidebar.update(cx, |s, cx| s.set_selected(sid, cx));
                self.terminal.update(cx, |t, cx| t.set_session(sid, cx));
                new_session = sid.is_some();
            }
            if old.transcript() != transcript {
                self.transcript
                    .update(cx, |t, cx| t.set_session(transcript, cx));
                self.composer
                    .update(cx, |c, cx| c.set_session(transcript, cx));
            }
            if old.sid != sid || old.cwd != cwd {
                self.files.update(cx, |f, cx| f.set_session(sid, &cwd, cx));
            }
        }
        if new_session {
            self.land_focus(window, cx);
        } else if self.want_focus {
            self.focus_centre(window, cx);
        }
        cx.notify();
    }

    /// The terminal panel attached or dropped a pane: the centre may have its pane now.
    fn terminal_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.want_focus {
            self.focus_centre(window, cx);
        }
        cx.notify();
    }

    /// The keyboard goes to the composer, or to the terminal that fills the centre. False when the
    /// centre has nothing to type into yet (a card, a terminal that is not attached): `want_focus`
    /// stays set, and the panel's next change retries.
    fn focus_centre(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let handle = match self.shown.centre.clone() {
            Centre::Transcript(_) => Some(self.composer.focus_handle(cx)),
            Centre::Terminal(sid) => self
                .terminal
                .read(cx)
                .main_pane(sid)
                .map(|pane| pane.focus_handle(cx)),
            Centre::Card(_) => None,
        };
        let Some(handle) = handle else {
            return false;
        };
        window.focus(&handle, cx);
        self.want_focus = false;
        self.check_focus = true;
        true
    }

    /// Called from `render`. A handle that no element carries leaves the keyboard on nothing, and then
    /// every key of the shell (ctrl-b, F6, ...) is dead until a click. So when a module's
    /// handle has just taken the keyboard, look once the frame that should carry it is out: if the
    /// shell does not contain the focus, the centre's own stop takes it back.
    fn verify_focus(&mut self, window: &mut Window) {
        if !std::mem::take(&mut self.check_focus) {
            return;
        }
        let (root, centre) = (self.focus.clone(), self.centre_focus.clone());
        window.on_next_frame(move |window, cx| {
            if !root.contains_focused(window, cx) {
                window.focus(&centre, cx);
            }
        });
    }

    /// The keyboard lost its owner (a panel closed, a session changed): the centre takes it. A centre
    /// with nothing to type into gets its region handle, because a focused handle that is not drawn
    /// leaves every key of the shell (ctrl-b, F6, ...) without its context.
    fn land_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.want_focus = true;
        if !self.focus_centre(window, cx) {
            window.focus(&self.centre_focus, cx);
        }
    }

    /// The sidebar's click: show that session.
    fn open_session(&mut self, sid: femme_proto::Sid, cx: &mut Context<Self>) {
        if let Some(net) = &self.net {
            net.update(cx, |n, cx| n.set_current(Some(sid), cx));
        }
    }

    fn on_menu(&mut self, ev: MenuEvent, _: &mut Window, cx: &mut Context<Self>) {
        match ev {
            MenuEvent::Accounts => {
                self.accounts.update(cx, |a, cx| a.open(cx));
                cx.notify();
            }
            MenuEvent::Reconnect => {
                if let Some(net) = &self.net {
                    net.read(cx).reconnect();
                }
            }
            MenuEvent::Quit => cx.quit(),
            // The menu says what the timer will do by itself.
            MenuEvent::CheckUpdates => {}
        }
    }

    /// The accounts card opened or closed. It takes the keyboard when it opens (and remembers who had
    /// it) and gives it back when the user dismisses it; when nobody had it, the centre takes it.
    fn accounts_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let open = self.accounts.read(cx).is_open();
        if open != self.modal_open {
            self.modal_open = open;
            let card = self.accounts.focus_handle(cx);
            if !open && window.focused(cx).is_none_or(|h| card.contains(&h, window)) {
                self.land_focus(window, cx);
            }
            cx.notify();
        }
    }

    /// mod+b: the sessions column collapses to its strip of dots, or expands again.
    fn toggle_sidebar(&mut self, cx: &mut Context<Self>) {
        self.sidebar.update(cx, |s, cx| {
            let collapsed = s.collapsed();
            s.set_collapsed(!collapsed, cx)
        });
    }

    /// F6 and shift+F6: sidebar, centre, right-hand panel (which the cycle opens).
    fn focus_cycle(&mut self, back: bool, window: &mut Window, cx: &mut Context<Self>) {
        let at = if self.sidebar.focus_handle(cx).contains_focused(window, cx) {
            Some(Stop::Sidebar)
        } else if self.right.open().is_some() && self.right_focus.contains_focused(window, cx) {
            Some(Stop::Right)
        } else if self.centre_focus.contains_focused(window, cx) {
            Some(Stop::Centre)
        } else {
            None
        };
        self.want_focus = false;
        match next_stop(at, back) {
            Stop::Sidebar => window.focus(&self.sidebar.focus_handle(cx), cx),
            Stop::Centre => {
                if !self.focus_centre(window, cx) {
                    window.focus(&self.centre_focus, cx);
                }
            }
            Stop::Right => {
                let tab = self.right.open().unwrap_or(self.right.last());
                self.open_right(tab, true, window, cx);
            }
        }
        cx.notify();
    }

    /// The top bar: the `femme` menu, then the link's status.
    fn top_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (status, tip): (SharedString, SharedString) = match &self.net {
            Some(net) => {
                let (line, whole) = status_of(net.read(cx));
                (line.into(), whole.into())
            }
            None => ("no daemon configured".into(), "no daemon configured".into()),
        };
        ruled(
            div()
                .flex_none()
                .h(TOP_BAR_H)
                .flex()
                .items_center()
                .gap(px(10.))
                .px(px(8.))
                .bg(SURFACE),
        )
        .child(
            div()
                .flex_none()
                .h_full()
                .flex()
                .items_center()
                .child(self.menu.clone()),
        )
        .child(
            ui::meta(status)
                .id("status")
                .flex_1()
                .min_w_0()
                .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx)),
        )
    }

    /// A paper card in the middle of the centre: why there is no session to show.
    fn card_view(title: &str, detail: &str) -> AnyElement {
        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .p(px(16.))
            .child(
                ui::card()
                    .max_w(px(560.))
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .child(
                        div()
                            .font_family(FONT_PIXEL)
                            .text_size(px(26.))
                            .line_height(relative(1.))
                            .child(title.to_owned()),
                    )
                    .when(!detail.is_empty(), |d| {
                        d.child(
                            div()
                                .text_size(px(14.))
                                .text_color(MUTED)
                                .child(detail.to_owned()),
                        )
                    }),
            )
            .into_any_element()
    }

    /// The centre: a transcript over its composer, a terminal, or a card.
    fn centre(&self, cx: &mut Context<Self>) -> AnyElement {
        let full = StyleRefinement::default().size_full();
        // The benchmark's synthetic terminals, side by side.
        if !self.panes.is_empty() {
            return div()
                .size_full()
                .flex()
                .gap(GUTTER)
                .children(self.panes.iter().map(|p| {
                    ruled(
                        div()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .bg(SURFACE)
                            .pl(px(8.))
                            .pr(px(4.))
                            .py(px(6.)),
                    )
                    .child(p.clone())
                }))
                .into_any_element();
        }
        match &self.shown.centre {
            Centre::Transcript(_) => div()
                .size_full()
                .flex()
                .flex_col()
                .gap(GUTTER)
                .child(
                    ruled(div().flex_1().min_h_0().bg(SURFACE).overflow_hidden())
                        .child(self.transcript.clone().cached(full)),
                )
                // The composer sizes itself and draws its own box.
                .child(div().flex_none().child(self.composer.clone()))
                .into_any_element(),
            Centre::Terminal(sid) => ruled(div().size_full().bg(SURFACE).overflow_hidden())
                .child(match self.terminal.read(cx).main_pane(*sid) {
                    Some(pane) => div()
                        .size_full()
                        .pl(px(8.))
                        .pr(px(4.))
                        .py(px(6.))
                        .child(pane)
                        .into_any_element(),
                    None => Self::card_view("starting…", "attaching to the session's terminal"),
                })
                .into_any_element(),
            Centre::Card(card) => ruled(div().size_full().bg(SURFACE))
                .child(Self::card_view(card.title(), &card.detail()))
                .into_any_element(),
        }
    }
}

impl Render for FemmeApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::bench::probe::frame();
        self.verify_focus(window);
        let viewport = f32::from(window.viewport_size().width);
        self.right.fit(viewport);
        // The benchmark's `--full-height`: the synthetic terminals alone, no frame.
        if self.cfg.full_height && !self.panes.is_empty() {
            return div()
                .id("femme")
                .size_full()
                .p(GUTTER)
                .bg(SURFACE)
                .text_color(INK)
                .child(self.centre(cx))
                .into_any_element();
        }
        // The sidebar draws its own box and says how wide it is; a cached view lays out as the style
        // it is given alone.
        let column = StyleRefinement::default()
            .h_full()
            .w(self.sidebar.read(cx).width());
        let body = div()
            .flex_1()
            .min_h_0()
            .flex()
            .gap(GUTTER)
            .child(self.sidebar.clone().cached(column))
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .track_focus(&self.centre_focus)
                    .child(self.centre(cx))
                    // The card is its own layer (scrim, focus, escape): it only needs a positioned parent.
                    .when(self.modal_open, |d| d.child(self.accounts.clone())),
            )
            .child(div().flex_none().h_full().child(self.strip(cx)));
        div()
            .id("femme")
            .key_context("Femme")
            .track_focus(&self.focus)
            .on_action(cx.listener(|t, _: &ToggleSidebar, _, cx| t.toggle_sidebar(cx)))
            .on_action(
                cx.listener(|t, _: &ToggleTerminal, window, cx| t.toggle_terminal(window, cx)),
            )
            .on_action(cx.listener(|t, _: &CloseRight, window, cx| t.close_right(window, cx)))
            .on_action(cx.listener(|t, _: &FocusNext, window, cx| t.focus_cycle(false, window, cx)))
            .on_action(cx.listener(|t, _: &FocusPrev, window, cx| t.focus_cycle(true, window, cx)))
            .size_full()
            .relative()
            .font_family(FONT_UI)
            .text_size(BODY_SIZE)
            .line_height(relative(1.4))
            .text_color(INK)
            .child(
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full()
                    .child(self.dots.clone()),
            )
            // Reversed, so the top bar is the last child: it paints over the body, and what the
            // menu opens below it stays on top.
            .child(
                div()
                    .size_full()
                    .flex()
                    .flex_col_reverse()
                    .gap(GUTTER)
                    .p(GUTTER)
                    .child(body)
                    .child(self.top_bar(cx)),
            )
            .when_some(self.right.open(), |d, tab| {
                d.child(self.right_panel(tab, cx))
            })
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_repeat_over_the_panes() {
        let cfg = Config {
            panes: 5,
            modes: vec![SourceKind::Fire, SourceKind::Cat],
            ..Config::default()
        };
        assert_eq!(cfg.mode_label(), "fire,cat,fire,cat,fire");
        assert_eq!(cfg.mode(3), SourceKind::Cat);
    }

    #[test]
    fn the_default_config_has_no_daemon_and_no_benchmark() {
        let cfg = Config::default();
        assert!(cfg.connect.is_none() && cfg.bench.is_none());
        assert!(!cfg.modes.is_empty());
    }
}
