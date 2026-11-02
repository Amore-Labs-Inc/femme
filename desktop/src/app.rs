//! The desktop shell: nav bar, three columns (sidebar | transcript + composer | right panel) over a
//! bottom terminal drawer, all in resizable panels, plus keybindings, focus cycling and the command
//! palette placeholder.
//!
//! Frame cost: the sidebar, right panel, composer and transcript are separate entities mounted with
//! `.cached(..)`, so a frame in which only the terminal panes changed replays their recorded paint
//! instead of rebuilding them. Only the shell itself (cheap: a few dozen divs) and the pane views
//! re-render per frame.

use std::collections::BTreeMap;
#[cfg(feature = "bench")]
use std::time::Duration;

use femme_proto::rpc::{Req, Resp};
use femme_proto::state::{Agent, Mode};
use femme_proto::{PaneId, Sid};
use gpui::prelude::FluentBuilder as _;
use gpui::{
    div, px, relative, size, App, AppContext as _, Context, Div, Entity, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, KeyBinding, MouseButton, ParentElement as _, Render,
    Stateful, StatefulInteractiveElement as _, StyleRefinement, Styled as _, Subscription, Task,
    TitlebarOptions, Window, WindowBounds, WindowOptions,
};
use gpui_component::{h_resizable, resizable_panel, v_resizable, Root};

#[cfg(feature = "bench")]
use crate::bench::{Bench, BenchConfig, PaneStats};
use crate::connect::Connect;
use crate::net::{Net, Started};
use crate::panels::{palette, Composer, OpenSession, RightPanel, Sidebar};
use crate::terminal::{SourceKind, TerminalPane};
use crate::theme::{self, BODY_SIZE, FONT_BRAND, FONT_UI, INK, NEON, PAPER};
use crate::transcript::TranscriptView;
use crate::ui::{button, button_dark, meta, ruled_sides, tb, win, DotGrid};

#[allow(missing_docs)]
mod actions_def {
    gpui::actions!(
        femme,
        [
            TogglePalette,
            ClosePalette,
            ToggleDrawer,
            ToggleSidebar,
            ToggleRight,
            FocusNext
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

/// Binds the shell's keys: mod+k palette, mod+j drawer, mod+b sidebar, mod+alt+b right panel,
/// F6 focus cycle, escape closes the palette.
pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new(modkey!("k"), TogglePalette, Some("Femme")),
        KeyBinding::new(modkey!("j"), ToggleDrawer, Some("Femme")),
        KeyBinding::new(modkey!("b"), ToggleSidebar, Some("Femme")),
        KeyBinding::new(modkey!("alt-b"), ToggleRight, Some("Femme")),
        KeyBinding::new("f6", FocusNext, Some("Femme")),
        KeyBinding::new("escape", ClosePalette, Some("Palette")),
    ]);
}

/// Everything the command line configures.
#[derive(Clone, Debug)]
pub struct Config {
    /// Run the benchmark for this many seconds, then print the report and exit.
    pub bench: Option<u64>,
    /// Terminal panes in the drawer.
    pub panes: usize,
    /// Transcript rows; 0 leaves the transcript out.
    pub rows: usize,
    /// Synthetic terminal frame rate.
    pub hz: u32,
    /// What the panes show, not empty: pane `i` takes `modes[i % modes.len()]`.
    pub modes: Vec<SourceKind>,
    /// The drawer takes the whole window (columns and transcript are left out), so the panes are as
    /// tall as the window allows.
    pub full_height: bool,
    /// Frame-time target in milliseconds for the benchmark verdict; `None` follows the display.
    pub target_ms: Option<f64>,
    /// Seed of every synthetic source.
    pub seed: u64,
    /// Requested window size in logical pixels (a compositor may override it).
    pub window: (f32, f32),
    /// The daemon to show; `None` runs on synthetic panes and mock sessions.
    pub connect: Option<Connect>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bench: None,
            panes: 4,
            rows: 50_000,
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
        rows: cfg.rows,
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

/// Terminals attached at start, all of them, up to this many panes; past it only the tab on show
/// has one (every attached terminal is a stream the daemon keeps feeding).
const EAGER: usize = 8;

/// The shell on a daemon: what the sidebar and the drawer show of it.
struct Live {
    net: Entity<Net>,
    /// The terminals of the panes that have a tab, attached once and kept while the pane lives.
    terms: BTreeMap<PaneId, Entity<TerminalPane>>,
    /// The tab on show.
    active: Option<PaneId>,
    /// A session this shell just created: its first pane is shown as soon as the daemon lists it.
    fresh: Option<Sid>,
    /// The tab "close" asked the daemon to close: when it goes, the keyboard moves to the next tab.
    closing: Option<PaneId>,
}

/// The shell view.
pub struct FemmeApp {
    cfg: Config,
    dots: DotGrid,
    focus: FocusHandle,
    palette_focus: FocusHandle,
    sidebar: Entity<Sidebar>,
    right: Entity<RightPanel>,
    composer: Entity<Composer>,
    transcript: Option<Entity<TranscriptView>>,
    panes: Vec<Entity<TerminalPane>>,
    show_sidebar: bool,
    show_right: bool,
    show_drawer: bool,
    palette_open: bool,
    live: Option<Live>,
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
        // The full-height drawer leaves no room for the transcript.
        let cfg = Config {
            rows: if cfg.full_height { 0 } else { cfg.rows },
            ..cfg
        };
        let transcript = (cfg.rows > 0)
            .then(|| cx.new(|cx| TranscriptView::new(cfg.rows, cfg.seed, &rt, window, cx)));
        let live = started.map(|s| Live {
            net: cx.new(|cx| Net::new(s.client, rt.clone(), s.enroll, cx)),
            terms: BTreeMap::new(),
            active: None,
            fresh: None,
            closing: None,
        });
        // With a daemon the panes are its own: no synthetic ones.
        let panes: Vec<Entity<TerminalPane>> = (0..cfg.panes)
            .filter(|_| live.is_none())
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
        let sidebar = cx.new(|cx| Sidebar::new(live.as_ref().map(|l| l.net.clone()), cx));
        let mut subs = Vec::new();
        if let Some(l) = &live {
            subs.push(cx.observe_in(&l.net, window, |t, _, window, cx| t.sync(window, cx)));
            subs.push(
                cx.subscribe_in(&sidebar, window, |t, _, ev: &OpenSession, window, cx| {
                    t.open_session(ev.0, window, cx)
                }),
            );
            // A window nobody looks at needs the daemon's attention less often.
            subs.push(cx.observe_window_activation(window, |t, window, cx| {
                if let Some(l) = &t.live {
                    l.net
                        .read(cx)
                        .client()
                        .set_background(!window.is_window_active());
                }
            }));
        }
        Self {
            dots: DotGrid::new(window.scale_factor(), cx),
            focus: cx.focus_handle(),
            palette_focus: cx.focus_handle(),
            sidebar,
            right: cx.new(RightPanel::new),
            composer: cx.new(Composer::new),
            transcript,
            panes,
            cfg,
            show_sidebar: true,
            show_right: true,
            show_drawer: true,
            palette_open: false,
            live,
            _subs: subs,
            _bench: bench,
        }
    }

    /// The terminals the drawer shows: every synthetic pane, or the tab on show.
    fn shown(&self) -> Vec<Entity<TerminalPane>> {
        match &self.live {
            None => self.panes.clone(),
            Some(l) => l
                .active
                .and_then(|p| l.terms.get(&p))
                .cloned()
                .into_iter()
                .collect(),
        }
    }

    /// Brings the drawer in line with the daemon's panes: a terminal for each tab that appeared
    /// (past [`EAGER`] only for the one on show), none for those that went, and a tab on show.
    fn sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(live) = &mut self.live else { return };
        let net = live.net.read(cx);
        let tabs = net.model.tabs();
        let (client, rt) = (net.client(), net.rt());
        let fresh = live
            .fresh
            .and_then(|sid| tabs.iter().copied().find(|p| p.sid == sid));
        let closed = live.closing.filter(|p| !tabs.contains(p));
        if closed.is_some() {
            live.closing = None;
        }
        if fresh.is_some() {
            live.fresh = None;
            live.active = fresh;
            self.show_drawer = true;
        } else if !live.active.is_some_and(|p| tabs.contains(&p)) {
            live.active = tabs.first().copied();
        }
        live.terms.retain(|p, _| tabs.contains(p));
        for p in &tabs {
            if (tabs.len() <= EAGER || Some(*p) == live.active) && !live.terms.contains_key(p) {
                let term = cx.new(|cx| TerminalPane::attach(client.clone(), *p, &rt, cx));
                live.terms.insert(*p, term);
            }
        }
        // The keyboard goes to a new session's terminal, and on to the next one when a tab is closed.
        let refocus = fresh.or_else(|| closed.and(live.active));
        if let Some(term) = refocus.and_then(|p| live.terms.get(&p)) {
            window.focus(&term.focus_handle(cx), cx);
        }
        cx.notify();
    }

    /// Shows tab `p` with the keyboard on its terminal.
    fn select(&mut self, p: PaneId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(live) = &mut self.live else { return };
        live.active = Some(p);
        self.show_drawer = true;
        self.sync(window, cx);
        if let Some(term) = self.live.as_ref().and_then(|l| l.terms.get(&p)) {
            window.focus(&term.focus_handle(cx), cx);
        }
    }

    /// The sidebar's click: the session's first terminal, if it has one.
    fn open_session(&mut self, sid: Sid, window: &mut Window, cx: &mut Context<Self>) {
        let first = self.live.as_ref().and_then(|l| {
            l.net
                .read(cx)
                .model
                .tabs()
                .into_iter()
                .find(|p| p.sid == sid)
        });
        if let Some(p) = first {
            self.select(p, window, cx);
        }
    }

    /// "+": a new shell session, started where the one on show is. It takes the drawer when the
    /// daemon lists it.
    fn new_session(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(live) = &self.live else { return };
        let cwd = live
            .active
            .and_then(|p| live.net.read(cx).model.sessions.get(&p.sid))
            .map(|s| s.cwd.clone())
            .unwrap_or_default();
        let req = Req::SessionCreate {
            cwd,
            agent: Agent::Shell,
            mode: Mode::Tui,
            label: None,
            name: None,
            prompt: None,
        };
        let sent = live.net.update(cx, |n, cx| n.run(req, cx));
        cx.spawn_in(window, async move |this, cx| {
            if let Some(Resp::Session(sid)) = sent.await {
                this.update_in(cx, |t, window, cx| {
                    if let Some(l) = &mut t.live {
                        l.fresh = Some(sid);
                    }
                    t.sync(window, cx);
                })
                .ok();
            }
        })
        .detach();
    }

    /// "close": the tab on show goes, with its session when it is the session's only pane.
    fn close_tab(&mut self, cx: &mut Context<Self>) {
        let Some(live) = &mut self.live else { return };
        let Some(p) = live.active else { return };
        live.closing = Some(p);
        let siblings = live
            .net
            .read(cx)
            .model
            .panes
            .keys()
            .filter(|q| q.sid == p.sid)
            .count();
        let req = if siblings <= 1 {
            Req::SessionClose { sid: p.sid }
        } else {
            Req::PaneClose { pane: p }
        };
        let sent = live.net.update(cx, |n, cx| n.run(req, cx));
        // A refused close leaves the tab where it is: nothing to follow.
        cx.spawn(async move |this, cx| {
            if sent.await.is_none() {
                this.update(cx, |t, _| {
                    if let Some(l) = &mut t.live {
                        l.closing = None;
                    }
                })
                .ok();
            }
        })
        .detach();
    }

    fn toggle_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.palette_open {
            self.close_palette(window, cx);
        } else {
            self.palette_open = true;
            window.focus(&self.palette_focus, cx);
            cx.notify();
        }
    }

    fn close_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.palette_open {
            self.palette_open = false;
            window.focus(&self.focus, cx);
            cx.notify();
        }
    }

    /// The focus stops F6 walks, in order: sidebar, transcript, composer, drawer (first pane).
    fn stops(&self, cx: &App) -> Vec<Vec<FocusHandle>> {
        let mut s = Vec::new();
        if self.show_sidebar {
            s.push(vec![self.sidebar.focus_handle(cx)]);
        }
        if let Some(t) = &self.transcript {
            s.push(vec![t.focus_handle(cx)]);
        }
        s.push(vec![self.composer.focus_handle(cx)]);
        let shown = self.shown();
        if self.show_drawer && !shown.is_empty() {
            s.push(shown.iter().map(|p| p.focus_handle(cx)).collect());
        }
        s
    }

    fn focus_next(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let stops = self.stops(cx);
        let cur = stops
            .iter()
            .position(|hs| hs.iter().any(|h| h.contains_focused(window, cx)));
        let next = cur.map_or(0, |i| (i + 1) % stops.len());
        window.focus(&stops[next][0], cx);
    }

    fn nav(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let status = match &self.live {
            Some(l) => l.net.read(cx).status_line(),
            None => format!(
                "{} pane(s) {}@{}Hz | {} transcript rows",
                self.cfg.panes,
                self.cfg.mode_label(),
                self.cfg.hz,
                self.cfg.rows
            ),
        };
        let toggle = |id: &'static str, label: &'static str, on: bool| {
            let b = if on {
                button_dark(id, label)
            } else {
                button(id, label)
            };
            b.h(px(30.))
        };
        ruled_sides(
            div()
                .flex_none()
                .flex()
                .items_center()
                .gap(px(10.))
                .px(px(12.))
                .py(px(4.))
                .bg(PAPER),
            px(0.),
            px(0.),
            theme::RULE,
            px(0.),
        )
        .child(
            // The wordmark: neon Kaushan Script with the 2px ink shadow the board uses (no text-shadow in gpui, so drawn twice).
            div()
                .relative()
                .w(px(96.))
                .h(px(40.))
                .font_family(FONT_BRAND)
                .text_size(px(32.))
                .line_height(relative(1.1))
                .child(
                    div()
                        .absolute()
                        .top(px(2.))
                        .left(px(2.))
                        .text_color(INK)
                        .child("femme"),
                )
                .child(
                    div()
                        .absolute()
                        .top_0()
                        .left_0()
                        .text_color(NEON)
                        .child("femme"),
                ),
        )
        .child(meta(status).flex_1().min_w_0())
        .child(
            toggle("n-sidebar", "sidebar", self.show_sidebar).on_click(cx.listener(
                |t, _, _, cx| {
                    t.show_sidebar = !t.show_sidebar;
                    cx.notify();
                },
            )),
        )
        .child(
            toggle("n-right", "panel", self.show_right).on_click(cx.listener(|t, _, _, cx| {
                t.show_right = !t.show_right;
                cx.notify();
            })),
        )
        .child(
            toggle("n-drawer", "terminal", self.show_drawer).on_click(cx.listener(
                |t, _, _, cx| {
                    t.show_drawer = !t.show_drawer;
                    cx.notify();
                },
            )),
        )
        .child(
            toggle("n-palette", "palette", self.palette_open)
                .on_click(cx.listener(|t, _, window, cx| t.toggle_palette(window, cx))),
        )
    }

    fn centre(&self) -> impl IntoElement {
        let full = StyleRefinement::default().size_full();
        div()
            .size_full()
            .p(px(3.))
            .flex()
            .flex_col()
            .gap(px(6.))
            .child(
                win()
                    .flex_1()
                    .min_h_0()
                    .child(tb("transcript - refactor auth middleware"))
                    .child(
                        div()
                            .flex_1()
                            .min_h_0()
                            .bg(PAPER)
                            .when_some(self.transcript.clone(), |d, t| {
                                d.child(t.cached(full.clone()))
                            })
                            .when(self.transcript.is_none(), |d| {
                                d.p(px(12.)).child(meta("transcript off (--rows 0)"))
                            }),
                    ),
            )
            .child(
                div()
                    .flex_none()
                    .h(px(132.))
                    .child(self.composer.clone().cached(full)),
            )
    }

    /// The synthetic panes' tabs: each focuses its pane (they are all on show).
    fn synthetic_tabs(&self, window: &Window, cx: &mut Context<Self>) -> Vec<Stateful<Div>> {
        self.panes
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let handle = p.focus_handle(cx);
                let label = p.read(cx).label();
                let b = if handle.contains_focused(window, cx) {
                    button_dark(("dtab", i), label)
                } else {
                    button(("dtab", i), label)
                };
                b.h(px(28.))
                    .on_click(move |_, window, cx| window.focus(&handle, cx))
            })
            .collect()
    }

    /// The daemon's tabs, one per pane of a live session, then "+" and "close".
    fn live_tabs(&self, live: &Live, cx: &mut Context<Self>) -> Vec<Stateful<Div>> {
        let net = live.net.read(cx);
        let mut tabs: Vec<_> = net
            .model
            .tabs()
            .into_iter()
            .enumerate()
            .map(|(i, p)| {
                let title = live.terms.get(&p).and_then(|t| t.read(cx).title());
                let label = net.model.tab_label(p, title);
                let b = if live.active == Some(p) {
                    button_dark(("dtab", i), label)
                } else {
                    button(("dtab", i), label)
                };
                b.h(px(28.))
                    .on_click(cx.listener(move |t, _, window, cx| t.select(p, window, cx)))
            })
            .collect();
        tabs.push(
            button("dnew", "+")
                .h(px(28.))
                .on_click(cx.listener(|t, _, window, cx| t.new_session(window, cx))),
        );
        if live.active.is_some() {
            tabs.push(
                button("dclose", "close")
                    .h(px(28.))
                    .on_click(cx.listener(|t, _, _, cx| t.close_tab(cx))),
            );
        }
        tabs
    }

    fn drawer(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (count, tabs) = match &self.live {
            None => (self.panes.len(), self.synthetic_tabs(window, cx)),
            Some(l) => (l.net.read(cx).model.tabs().len(), self.live_tabs(l, cx)),
        };
        let shown = self.shown();
        let body = div().flex_1().min_h_0();
        let body = if shown.is_empty() {
            body.p(px(12.)).child(meta("no terminal: + starts a shell"))
        } else {
            body.child(h_resizable("panes").children(shown.iter().map(|p| {
                resizable_panel().child(crate::ui::ruled(
                    div()
                        .size_full()
                        .bg(PAPER)
                        .pl(px(8.))
                        .pr(px(4.))
                        .py(px(6.))
                        .child(p.clone()),
                ))
            })))
        };
        div().size_full().p(px(3.)).child(
            win()
                .size_full()
                .child(tb(format!("terminal drawer - {count} pane(s)")))
                .child(
                    div()
                        .flex_none()
                        .bg(theme::PANEL)
                        .p(px(3.))
                        .child(div().flex().gap(px(3.)).children(tabs)),
                )
                .child(body),
        )
    }
}

impl Render for FemmeApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::bench::probe::frame();
        let full = StyleRefinement::default().size_full();
        let columns = h_resizable("cols")
            .child(
                resizable_panel()
                    .visible(self.show_sidebar)
                    .size(px(280.))
                    .size_range(px(200.)..px(440.))
                    .child(
                        div()
                            .size_full()
                            .p(px(3.))
                            .child(self.sidebar.clone().cached(full.clone())),
                    ),
            )
            .child(resizable_panel().child(self.centre()))
            .child(
                resizable_panel()
                    .visible(self.show_right)
                    .size(px(340.))
                    .size_range(px(240.)..px(620.))
                    .child(
                        div()
                            .size_full()
                            .p(px(3.))
                            .child(self.right.clone().cached(full)),
                    ),
            );
        // Full height: the columns step aside and the drawer, with no size of its own, takes the body.
        let full = self.cfg.full_height;
        let drawer = resizable_panel().visible(self.show_drawer);
        let drawer = if full {
            drawer
        } else {
            drawer.size(px(320.)).size_range(px(140.)..px(720.))
        };
        let body = v_resizable("main")
            .child(resizable_panel().visible(!full).child(columns))
            .child(drawer.child(self.drawer(window, cx)));
        let this = cx.entity();
        div()
            .id("femme")
            .key_context("Femme")
            .track_focus(&self.focus)
            .on_action(cx.listener(|t, _: &TogglePalette, window, cx| t.toggle_palette(window, cx)))
            .on_action(cx.listener(|t, _: &ClosePalette, window, cx| t.close_palette(window, cx)))
            .on_action(cx.listener(|t, _: &ToggleDrawer, _, cx| {
                t.show_drawer = !t.show_drawer;
                cx.notify();
            }))
            .on_action(cx.listener(|t, _: &ToggleSidebar, _, cx| {
                t.show_sidebar = !t.show_sidebar;
                cx.notify();
            }))
            .on_action(cx.listener(|t, _: &ToggleRight, _, cx| {
                t.show_right = !t.show_right;
                cx.notify();
            }))
            .on_action(cx.listener(|t, _: &FocusNext, window, cx| t.focus_next(window, cx)))
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
            .child(
                div()
                    .size_full()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .p(px(6.))
                    .child(self.nav(cx))
                    .child(div().flex_1().min_h_0().child(body)),
            )
            .when(self.palette_open, |d| {
                d.child(
                    // Taffy resolves absolute children against their direct parent, so this layer is
                    // itself pinned to the whole window for the palette's scrim to fill.
                    div()
                        .absolute()
                        .inset_0()
                        .key_context("Palette")
                        .track_focus(&self.palette_focus)
                        .on_mouse_down(MouseButton::Left, |_, _, _| {})
                        .child(palette(move |window, cx| {
                            this.update(cx, |t, cx| t.close_palette(window, cx));
                        })),
                )
            })
    }
}
