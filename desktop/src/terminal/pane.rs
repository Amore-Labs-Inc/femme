//! A terminal pane: the view entity that owns a [`RowCache`] (the one the real client uses, from
//! `femme-term`), applies the frames a holder streams to it and renders them with
//! [`TerminalElement`]. The holder is a synthetic one ([`spawn_source`]) or the daemon's
//! ([`TerminalPane::attach`]); the pane cannot tell them apart.
//!
//! Threading: the holder runs on tokio worker threads and sends owned [`TermDown`]s over a channel
//! bounded at its unacked window; the pump task moves them into the pane's inbox and asks for a
//! redraw. Frames are applied, and acked, when the pane is drawn (`render`): a pane nobody draws
//! (hidden window, closed drawer) stops acking, so the holder stops at its window and merges
//! instead of the UI applying frames nobody sees. Nothing on the UI thread awaits the holder.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::Arc;
use std::time::Duration;

use femme_client::Client;
use femme_proto::term::{Frame, Mods, MouseEv, MouseKind, RowPatch, TermDown, TermUp};
use femme_proto::{Id, PaneId};
use femme_term::client::{ApplyError, RowCache};
use gpui::{
    div, App, Context, FocusHandle, Focusable, InteractiveElement as _, IntoElement, KeyDownEvent,
    MouseButton, ParentElement as _, Pixels, Point, Render, ScrollEvent, SharedString, Styled as _,
    Task, Window,
};
use tokio::sync::mpsc;

use super::bridge::spawn_term;
use super::element::TerminalElement;
use super::input;
use super::runs::{BgRun, Palette};
use super::synth::{Fire, FrameSource, Holder, ShellScroll, WINDOW};
use crate::bench::PaneStats;

/// Which synthetic terminal feeds a pane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceKind {
    /// DOOM fire: every cell changes every frame (renderer worst case).
    Fire,
    /// Shell output scrolling one line per frame (the delta path).
    Shell,
    /// `cat` of random data: a whole screen of new text per frame (the text-shaping worst case).
    Cat,
}

impl SourceKind {
    /// Parses `fire` / `shell` / `cat`.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "fire" => Some(Self::Fire),
            "shell" => Some(Self::Shell),
            "cat" => Some(Self::Cat),
            _ => None,
        }
    }

    /// Lowercase name.
    pub fn name(self) -> &'static str {
        match self {
            Self::Fire => "fire",
            Self::Shell => "shell",
            Self::Cat => "cat",
        }
    }

    fn build(self, cols: u16, rows: u16, seed: u64) -> Box<dyn FrameSource> {
        match self {
            Self::Fire => Box::new(Fire::new(cols, rows, seed)),
            Self::Shell => Box::new(ShellScroll::new(cols, rows, seed)),
            Self::Cat => Box::new(ShellScroll::cat(cols, rows, seed)),
        }
    }
}

/// Cell grid metrics of the terminal font, measured on first paint.
#[derive(Clone, Copy, Debug)]
pub struct Metrics {
    /// Width of one cell.
    pub cell_w: Pixels,
    /// Height of one row.
    pub line_h: Pixels,
    /// Window scale factor they were measured at.
    pub scale: f32,
}

/// Everything the element needs to paint a pane, shared between the view and its element.
pub struct PaneState {
    /// Rows and screen state.
    pub cache: RowCache,
    /// Wire colour to RGB mapping.
    pub palette: Palette,
    /// Scratch: background runs of the frame being painted, with their row.
    pub bgs: Vec<(u16, BgRun)>,
    /// Grid metrics, once measured.
    pub metrics: Option<Metrics>,
    /// Where the grid's top left was last painted: the origin of the cell under the pointer.
    pub origin: Point<Pixels>,
    /// Grid size last requested from the holder.
    pub wanted: (u16, u16),
    /// How many rows the view is scrolled back from the live screen (0: it follows the output).
    pub scroll: u64,
    /// Frames applied.
    pub applied: u64,
    /// Times the stream was re-opened because a frame did not apply (a rev gap).
    pub resyncs: u64,
    /// How the program ended, once it has.
    pub exited: Option<i32>,
    /// Frames received and not applied yet: the holder keeps at most [`WINDOW`] unacked.
    inbox: VecDeque<Frame>,
    /// An `Open` is out and its snapshot has not come yet: deltas sent before it are dropped.
    resyncing: bool,
    /// The pane has the keyboard (told to the program, which may have asked to hear it).
    focused: bool,
    /// Wheel travel that has not added up to a step yet.
    wheel_acc: f32,
    pane: PaneId,
    up: mpsc::UnboundedSender<TermUp>,
}

impl PaneState {
    /// A pane attached to the stream behind `up`: the first thing it says is `Open`.
    fn new(pane: PaneId, up: mpsc::UnboundedSender<TermUp>, initial: (u16, u16)) -> Self {
        let mut st = Self {
            cache: RowCache::new(),
            palette: Palette::femme_light(),
            bgs: Vec::new(),
            metrics: None,
            origin: Point::default(),
            wanted: initial,
            scroll: 0,
            applied: 0,
            resyncs: 0,
            exited: None,
            inbox: VecDeque::new(),
            resyncing: false,
            focused: false,
            wheel_acc: 0.,
            pane,
            up,
        };
        st.open();
        st
    }

    /// Asks the holder for a `cols x rows` grid if that differs from what was last asked.
    pub fn want_size(&mut self, cols: u16, rows: u16) {
        if (cols, rows) != self.wanted {
            self.wanted = (cols, rows);
            self.send(TermUp::Resize { cols, rows });
        }
    }

    /// Says `up` to the holder (input, `Focus`, `GetRows`); gone with the holder, it is dropped.
    pub fn send(&self, up: TermUp) {
        let _ = self.up.send(up);
    }

    /// The pane gained or lost the keyboard: a program that asked (`CSI ? 1004 h`) hears it.
    pub fn set_focus(&mut self, on: bool) {
        if self.focused != on {
            self.focused = on;
            self.send(TermUp::Focus(on));
        }
    }

    /// What the holder sent.
    fn down(&mut self, msg: TermDown) {
        match msg {
            TermDown::Frame(f) => self.push(f),
            TermDown::Rows(rows) => self.rows(rows),
            TermDown::Exited { status } => self.exited = Some(status),
            // ponytail: no bell; a flash on the tab would do.
            TermDown::Bell => {}
        }
    }

    /// A frame arrived from the holder. It waits until the pane is drawn.
    fn push(&mut self, f: Frame) {
        self.inbox.push_back(f);
    }

    /// Applies, and acks, every frame that arrived since the last draw.
    fn drain(&mut self) {
        if self.inbox.is_empty() {
            return;
        }
        let probe = crate::bench::probe::enabled().then(std::time::Instant::now);
        while let Some(f) = self.inbox.pop_front() {
            self.apply(&f);
        }
        if let Some(t0) = probe {
            crate::bench::probe::apply_since(t0);
        }
    }

    /// Applies one frame and acks it. A frame from another holder instance (the pty was recreated)
    /// resets the cache and applies to the fresh one, since the new holder's first frame is a
    /// snapshot; a frame that does not apply otherwise re-opens the stream. Any frame snaps the view
    /// back to the live screen.
    fn apply(&mut self, f: &Frame) {
        if self.resyncing && f.base_rev != 0 {
            return; // sent before our `Open` got there: the snapshot is on its way
        }
        // ponytail: `RowCache::apply` clones the rows out of a borrowed frame; an owned
        // `apply(Frame)` in femme-term would move them in (follow-up for its owner).
        let mut result = self.cache.apply(f);
        if result == Err(ApplyError::EpochChanged) {
            self.cache = RowCache::new();
            result = self.cache.apply(f);
        }
        match result {
            Ok(()) => {
                self.resyncing = false;
                self.applied += 1;
                self.scroll = 0;
                self.send(TermUp::Ack {
                    rev: self.cache.rev,
                });
            }
            Err(_) => {
                self.resyncs += 1;
                self.open();
            }
        }
    }

    /// Says `Open` with what the cache holds; the answer is a snapshot.
    fn open(&mut self) {
        self.resyncing = true;
        self.send(TermUp::Open {
            pane: self.pane,
            epoch: self.cache.epoch,
            have_rev: self.cache.rev,
            // ponytail: the process id; a real client keeps a random id across restarts.
            client_instance: u128::from(std::process::id()),
        });
    }

    /// The scrollback rows on show: absolute numbers `[from, to)` above the live screen.
    fn shown(&self) -> (u64, u64) {
        let from = self.cache.top_abs.saturating_sub(self.scroll);
        let to = from.saturating_add(u64::from(self.wanted.1));
        (from, to.min(self.cache.top_abs))
    }

    /// Asks the holder for the rows on show that the cache lacks (it only holds what scrolled past
    /// while attached).
    fn want_rows(&self) {
        let (from, to) = self.shown();
        if let Some(missing) = self.cache.missing_after(from, to) {
            self.send(TermUp::GetRows {
                from_abs: missing,
                to_abs: to,
            });
        }
    }

    /// Scrollback rows the holder sent. An answer is capped and can stop short of the request: ask
    /// on from where it stopped, as long as it got somewhere.
    fn rows(&mut self, rows: Vec<RowPatch>) {
        let (from, to) = self.shown();
        let before = self.cache.missing_after(from, to);
        self.cache.add_rows(rows);
        let after = self.cache.missing_after(from, to);
        if after.is_some() && after != before {
            self.want_rows();
        }
    }

    /// Scrolls the view `lines` back (negative: forward) through the rows the holder keeps; whether
    /// it moved.
    fn scroll_by(&mut self, lines: i32) -> bool {
        let room = self.cache.top_abs.saturating_sub(self.cache.history_lo_abs);
        let to = (self.scroll as i64 + i64::from(lines)).clamp(0, room as i64) as u64;
        let moved = to != self.scroll;
        self.scroll = to;
        if moved {
            self.want_rows();
        }
        moved
    }

    /// A wheel turn of `lines` (positive: up) over `at`. A program that reports the mouse, or sits
    /// on the alternate screen where the holder turns a notch into arrow keys, hears it as one
    /// notch per three lines; otherwise it scrolls the view. Whether the pane needs a redraw.
    pub fn wheel(&mut self, lines: f32, at: Point<Pixels>, mods: Mods) -> bool {
        if input::wheel_is_the_programs(self.cache.modes) {
            let notches = input::steps(&mut self.wheel_acc, lines, 3.).clamp(-10, 10);
            let (cols, rows) = (self.cache.cols, self.cache.rows);
            let (col, row) = self.metrics.map_or((0, 0), |m| {
                input::cell(at, self.origin, (m.cell_w, m.line_h), (cols, rows))
            });
            let kind = if notches > 0 {
                MouseKind::ScrollUp
            } else {
                MouseKind::ScrollDown
            };
            for _ in 0..notches.abs() {
                let ev = MouseEv {
                    kind,
                    button: 0,
                    col,
                    row,
                    mods,
                };
                self.send(TermUp::Mouse { seq: 0, ev });
            }
            false
        } else {
            let steps = input::steps(&mut self.wheel_acc, lines, 1.);
            steps != 0 && self.scroll_by(steps)
        }
    }
}

/// Spawns `src` as a holder on the side runtime, ticking at `hz` with its first tick `phase` (0..1) of
/// a period after the start, so several sources can be staggered instead of all landing in the same
/// frame. Returns what the holder says (bounded at its unacked window) and the channel for what the
/// client says upward: `Open` first (nothing is sent before it), then `Ack`s and `Resize`s (the
/// next frame after a resize is a full snapshot).
pub fn spawn_source(
    rt: &tokio::runtime::Handle,
    src: Box<dyn FrameSource>,
    hz: u32,
    phase: f32,
) -> (mpsc::Receiver<TermDown>, mpsc::UnboundedSender<TermUp>) {
    let (tx, rx) = mpsc::channel(WINDOW);
    let (up_tx, mut up_rx) = mpsc::unbounded_channel();
    rt.spawn(async move {
        let mut holder = Holder::new(src);
        let period = Duration::from_secs_f64(1.0 / f64::from(hz.max(1)));
        let mut tick = tokio::time::interval_at(
            tokio::time::Instant::now() + period.mul_f32(phase.clamp(0.0, 1.0)),
            period,
        );
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            let frame = tokio::select! {
                _ = tick.tick() => holder.tick(),
                msg = up_rx.recv() => match msg {
                    Some(m) => holder.up(m),
                    None => break,
                },
            };
            if let Some(f) = frame {
                if tx.send(TermDown::Frame(f)).await.is_err() {
                    break;
                }
            }
        }
    });
    (rx, up_tx)
}

/// One terminal pane view.
pub struct TerminalPane {
    state: Rc<RefCell<PaneState>>,
    focus: FocusHandle,
    label: SharedString,
    refused: Arc<AtomicU64>,
    _pump: Task<()>,
}

impl TerminalPane {
    /// A pane fed by a fresh `kind` source at `hz` on the runtime behind `rt`.
    pub fn new(
        id: usize,
        kind: SourceKind,
        hz: u32,
        seed: u64,
        phase: f32,
        rt: &tokio::runtime::Handle,
        cx: &mut Context<Self>,
    ) -> Self {
        // The source starts at a guess; the first paint asks for the real grid.
        let initial = (100, 30);
        let (rx, up) = spawn_source(rt, kind.build(initial.0, initial.1, seed), hz, phase);
        let pane = PaneId {
            sid: Id(1),
            no: id as u16,
        };
        let label = format!("{} {}", id + 1, kind.name()).into();
        Self::start(pane, rx, up, initial, label, Arc::default(), cx)
    }

    /// A pane of the daemon, attached through `client`: a bridge on the side runtime owns the
    /// client's `Term`, so the pane sees the same channels a synthetic source gives it.
    pub fn attach(
        client: Arc<Client>,
        pane: PaneId,
        rt: &tokio::runtime::Handle,
        cx: &mut Context<Self>,
    ) -> Self {
        let refused = Arc::new(AtomicU64::new(0));
        let (rx, up) = spawn_term(rt, client, pane, refused.clone());
        // A 0x0 grid: the pty is as big as the last client left it, so the first paint always says
        // its own size.
        Self::start(pane, rx, up, (0, 0), SharedString::default(), refused, cx)
    }

    fn start(
        pane: PaneId,
        mut rx: mpsc::Receiver<TermDown>,
        up: mpsc::UnboundedSender<TermUp>,
        initial: (u16, u16),
        label: SharedString,
        refused: Arc<AtomicU64>,
        cx: &mut Context<Self>,
    ) -> Self {
        let state = Rc::new(RefCell::new(PaneState::new(pane, up, initial)));
        let pump_state = state.clone();
        let pump = cx.spawn(async move |this, cx| {
            while let Some(msg) = rx.recv().await {
                let alive = this
                    .update(cx, |_, cx| {
                        pump_state.borrow_mut().down(msg);
                        cx.notify();
                    })
                    .is_ok();
                if !alive {
                    break;
                }
            }
        });
        Self {
            state,
            focus: cx.focus_handle(),
            label,
            refused,
            _pump: pump,
        }
    }

    /// Short label for a synthetic pane's drawer tab.
    pub fn label(&self) -> SharedString {
        self.label.clone()
    }

    /// The title the program set, if it has.
    pub fn title(&self) -> Option<String> {
        let st = self.state.borrow();
        st.cache.title.clone().filter(|t| !t.is_empty())
    }

    /// How the program ended, once it has.
    pub fn exited(&self) -> Option<i32> {
        self.state.borrow().exited
    }

    /// Inputs the client refused because too much of it was still unconfirmed (a long outage).
    pub fn refused(&self) -> u64 {
        self.refused.load(Relaxed)
    }

    /// What the bench report says about this pane.
    pub fn stats(&self) -> PaneStats {
        let st = self.state.borrow();
        PaneStats {
            label: self.label.to_string(),
            cols: st.cache.cols,
            rows: st.cache.rows,
            applied: st.applied,
            resyncs: st.resyncs,
        }
    }

    /// A key goes to the program; anything that is not the program's (the app's chords) goes on to
    /// the shell. Paste reads the clipboard.
    fn key_down(&mut self, ev: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        let up = if input::is_paste(ks) {
            cx.read_from_clipboard()
                .and_then(|c| c.text())
                .map(|text| TermUp::Paste { seq: 0, text })
        } else {
            input::key(ks).map(|key| TermUp::Key { seq: 0, key })
        };
        match up {
            Some(up) => {
                self.state.borrow().send(up);
                cx.stop_propagation();
            }
            None => cx.propagate(),
        }
    }

    fn scrolled(&mut self, ev: &ScrollEvent, window: &mut Window, cx: &mut Context<Self>) {
        let mut st = self.state.borrow_mut();
        let line_h = st
            .metrics
            .map_or_else(|| window.line_height(), |m| m.line_h);
        let lines = ev.delta.pixel_delta(line_h).y / line_h;
        if st.wheel(lines, ev.position, input::mods(&ev.modifiers)) {
            cx.notify();
        }
    }
}

impl Focusable for TerminalPane {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for TerminalPane {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        {
            let mut st = self.state.borrow_mut();
            st.drain();
            st.set_focus(self.focus.is_focused(window) && window.is_window_active());
        }
        div()
            .size_full()
            .track_focus(&self.focus)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| window.focus(&this.focus, cx)),
            )
            .on_key_down(cx.listener(Self::key_down))
            .on_scroll(cx.listener(Self::scrolled))
            .child(TerminalElement::new(self.state.clone(), self.focus.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use femme_proto::term::Modes;
    use gpui::{point, px};

    fn pane() -> PaneId {
        PaneId { sid: Id(1), no: 0 }
    }

    fn state(initial: (u16, u16)) -> (PaneState, mpsc::UnboundedReceiver<TermUp>) {
        let (tx, mut up) = mpsc::unbounded_channel();
        let st = PaneState::new(pane(), tx, initial);
        // The attach: the first thing the pane says.
        assert!(matches!(
            up.try_recv(),
            Ok(TermUp::Open {
                epoch: 0,
                have_rev: 0,
                ..
            })
        ));
        (st, up)
    }

    /// Everything the pane said since the last look.
    fn said(up: &mut mpsc::UnboundedReceiver<TermUp>) -> Vec<TermUp> {
        std::iter::from_fn(|| up.try_recv().ok()).collect()
    }

    /// A pane wired to a holder on the same thread.
    struct Rig {
        st: PaneState,
        up: mpsc::UnboundedReceiver<TermUp>,
        holder: Holder,
        /// `Open`s the holder has been sent.
        opens: u32,
    }

    impl Rig {
        /// A pane attached to a holder running `src`, drawn once (the snapshot is applied).
        fn new(src: Box<dyn FrameSource>) -> Self {
            let (tx, up) = mpsc::unbounded_channel();
            let mut rig = Rig {
                st: PaneState::new(pane(), tx, (40, 10)),
                up,
                holder: Holder::new(src),
                opens: 0,
            };
            rig.draw();
            rig
        }

        /// The holder's program writes one step. A frame it sends reaches the pane unless `lost`.
        fn tick(&mut self, lost: bool) {
            if let Some(f) = self.holder.tick() {
                if !lost {
                    self.st.push(f);
                }
            }
            self.draw();
        }

        /// The pane is drawn; what it says goes to the holder and the answers come back, until quiet.
        fn draw(&mut self) {
            loop {
                self.st.drain();
                let mut answered = false;
                while let Ok(msg) = self.up.try_recv() {
                    self.opens += u32::from(matches!(msg, TermUp::Open { .. }));
                    if let Some(f) = self.holder.up(msg) {
                        self.st.push(f);
                        answered = true;
                    }
                }
                if !answered {
                    return;
                }
            }
        }
    }

    /// A snapshot of a 40x10 screen whose top row is `top_abs`, with scrollback from `history_lo_abs`.
    fn screen(top_abs: u64, history_lo_abs: u64) -> Frame {
        Frame {
            epoch: 1,
            rev: 1,
            cols: 40,
            rows: 10,
            top_abs,
            history_lo_abs,
            ..Frame::default()
        }
    }

    fn row(abs: u64) -> RowPatch {
        RowPatch {
            abs,
            runs: vec![(0, format!("row {abs}"))],
            ..RowPatch::default()
        }
    }

    fn get_rows(from_abs: u64, to_abs: u64) -> TermUp {
        TermUp::GetRows { from_abs, to_abs }
    }

    #[test]
    fn want_size_sends_only_changes() {
        let (mut st, mut up) = state((100, 30));
        st.want_size(100, 30);
        assert!(up.try_recv().is_err());
        st.want_size(120, 40);
        st.want_size(120, 40);
        assert_eq!(
            up.try_recv(),
            Ok(TermUp::Resize {
                cols: 120,
                rows: 40
            })
        );
        assert!(up.try_recv().is_err());
    }

    #[test]
    fn pane_acks_every_applied_frame() {
        let (mut st, mut up) = state((40, 10));
        let mut fire = Fire::new(40, 10, 7);
        for _ in 0..3 {
            st.push(fire.tick());
        }
        st.drain();
        let acks: Vec<TermUp> = std::iter::from_fn(|| up.try_recv().ok()).collect();
        assert_eq!(acks, [1, 2, 3].map(|rev| TermUp::Ack { rev }));
        assert_eq!((st.applied, st.resyncs), (3, 0));
        assert_eq!((st.cache.cols, st.cache.rows), (40, 10));
    }

    #[test]
    fn frames_wait_in_the_inbox_until_the_pane_is_drawn() {
        let (mut st, mut up) = state((40, 10));
        let mut fire = Fire::new(40, 10, 7);
        for _ in 0..WINDOW {
            st.push(fire.tick());
        }
        assert_eq!(st.applied, 0);
        assert!(
            up.try_recv().is_err(),
            "an undrawn pane acks nothing, so the holder stops at its window"
        );
        st.drain();
        assert_eq!(st.applied, WINDOW as u64);
    }

    #[test]
    fn pane_reopens_the_stream_after_a_lost_frame() {
        let mut rig = Rig::new(Box::new(Fire::new(40, 10, 1)));
        for _ in 0..3 {
            rig.tick(false);
        }
        rig.tick(true); // lost on the way
        let before = rig.st.applied;
        rig.tick(false); // chains to the lost one: a gap
        assert_eq!(rig.st.resyncs, 1);
        assert_eq!(rig.opens, 2, "the pane asked for the snapshot");
        assert!(
            rig.st.applied > before,
            "the snapshot it asked for was applied"
        );
        let rev = rig.st.cache.rev;
        rig.tick(false);
        assert_eq!(
            (rig.st.resyncs, rig.st.cache.rev),
            (1, rev + 1),
            "deltas apply again"
        );
    }

    #[test]
    fn a_holder_restart_resets_the_cache_without_a_reopen() {
        let mut rig = Rig::new(Box::new(ShellScroll::new(40, 10, 1)));
        for _ in 0..3 {
            rig.tick(false);
        }
        let epoch = rig.st.cache.epoch;
        rig.holder.restart();
        rig.tick(false); // the new holder's first frame: a snapshot of another epoch
        assert_ne!(
            rig.st.cache.epoch, epoch,
            "the new holder's snapshot was applied"
        );
        assert_eq!(
            (rig.st.resyncs, rig.opens),
            (0, 1),
            "a fresh cache takes it, nothing is re-opened"
        );
        rig.tick(false);
        assert_eq!((rig.st.resyncs, rig.opens), (0, 1));
    }

    #[test]
    fn a_frame_of_another_epoch_that_is_not_a_snapshot_resyncs() {
        let (mut st, mut up) = state((40, 10));
        let mut fire = Fire::new(40, 10, 7);
        for _ in 0..3 {
            st.push(fire.tick());
        }
        st.drain();
        said(&mut up);
        let mut stray = fire.tick();
        assert_ne!(stray.base_rev, 0);
        stray.epoch += 1;
        st.push(stray);
        st.drain();
        assert_eq!((st.resyncs, st.cache.rev), (1, 0), "the old screen is gone");
        assert!(matches!(
            said(&mut up)[..],
            [TermUp::Open {
                epoch: 0,
                have_rev: 0,
                ..
            }]
        ));
    }

    #[tokio::test]
    async fn source_answers_open_and_resizes() {
        let rt = tokio::runtime::Handle::current();
        let (mut frames, up) = spawn_source(&rt, SourceKind::Fire.build(20, 8, 1), 200, 0.0);
        let frame = |d: Option<TermDown>| match d {
            Some(TermDown::Frame(f)) => f,
            other => panic!("expected a frame, got {other:?}"),
        };
        up.send(TermUp::Open {
            pane: pane(),
            epoch: 0,
            have_rev: 0,
            client_instance: 1,
        })
        .unwrap();
        let first = frame(frames.recv().await);
        assert_eq!((first.cols, first.rows, first.base_rev), (20, 8, 0));
        up.send(TermUp::Resize { cols: 30, rows: 12 }).unwrap();
        let snap = loop {
            let f = frame(frames.recv().await);
            up.send(TermUp::Ack { rev: f.rev }).unwrap();
            if f.cols == 30 {
                break f;
            }
        };
        assert_eq!((snap.rows, snap.base_rev), (12, 0));
    }

    #[test]
    fn source_kind_parses() {
        assert_eq!(SourceKind::parse("fire"), Some(SourceKind::Fire));
        assert_eq!(SourceKind::parse("shell"), Some(SourceKind::Shell));
        assert_eq!(SourceKind::parse("cat"), Some(SourceKind::Cat));
        assert_eq!(SourceKind::parse("x"), None);
    }

    #[test]
    fn the_wheel_scrolls_back_and_asks_for_the_rows_the_cache_lacks() {
        let (mut st, mut up) = state((40, 10));
        // 50 rows of scrollback, none of them in the cache.
        st.push(screen(100, 50));
        st.drain();
        said(&mut up);
        let at = point(px(0.), px(0.));
        assert!(st.wheel(3., at, Mods::empty()));
        assert_eq!(st.scroll, 3);
        assert_eq!(said(&mut up), [get_rows(97, 100)]);
        // The answer stops short: the rest is asked for.
        st.down(TermDown::Rows(vec![row(97), row(98)]));
        assert_eq!(said(&mut up), [get_rows(99, 100)]);
        st.down(TermDown::Rows(vec![row(99)]));
        assert!(said(&mut up).is_empty(), "everything on show is held");
        assert_eq!(st.cache.row(98).map(|r| r.abs), Some(98));
        // An empty answer is the end of what the holder has: no asking again.
        st.wheel(10., at, Mods::empty());
        assert_eq!(said(&mut up), [get_rows(87, 97)]);
        st.down(TermDown::Rows(Vec::new()));
        assert!(said(&mut up).is_empty());
        // The view stays inside the scrollback, and a wheel the other way comes back.
        st.wheel(1000., at, Mods::empty());
        assert_eq!(st.scroll, 50);
        assert!(!st.wheel(1., at, Mods::empty()), "at the top already");
        assert!(st.wheel(-1000., at, Mods::empty()));
        assert_eq!(st.scroll, 0);
        assert!(!st.wheel(-1., at, Mods::empty()), "at the bottom already");
    }

    #[test]
    fn a_new_frame_snaps_the_view_back_to_the_live_screen() {
        let (mut st, _up) = state((40, 10));
        st.push(screen(100, 50));
        st.drain();
        st.wheel(5., point(px(0.), px(0.)), Mods::empty());
        assert_eq!(st.scroll, 5);
        st.push(Frame {
            rev: 2,
            base_rev: 1,
            top_abs: 101,
            ..screen(101, 50)
        });
        st.drain();
        assert_eq!((st.scroll, st.applied), (0, 2));
    }

    #[test]
    fn the_wheel_goes_to_a_program_that_reports_the_mouse() {
        let (mut st, mut up) = state((40, 10));
        st.push(screen(100, 50));
        st.drain();
        st.cache.modes = Modes::MOUSE_CLICK | Modes::SGR_MOUSE;
        st.metrics = Some(Metrics {
            cell_w: px(10.),
            line_h: px(20.),
            scale: 1.,
        });
        st.origin = point(px(100.), px(50.));
        said(&mut up);
        let at = point(px(135.), px(111.));
        let notch = |kind| TermUp::Mouse {
            seq: 0,
            ev: MouseEv {
                kind,
                button: 0,
                col: 3,
                row: 3,
                mods: Mods::SHIFT,
            },
        };
        assert!(!st.wheel(1., at, Mods::SHIFT), "a notch is three lines");
        assert!(said(&mut up).is_empty());
        st.wheel(2., at, Mods::SHIFT);
        st.wheel(-3., at, Mods::SHIFT);
        assert_eq!(
            said(&mut up),
            [notch(MouseKind::ScrollUp), notch(MouseKind::ScrollDown)]
        );
        assert_eq!(st.scroll, 0, "the view did not move");
        // On the alternate screen with alternate scroll the holder makes arrow keys of it.
        st.cache.modes = Modes::ALT_SCREEN | Modes::ALTERNATE_SCROLL;
        st.wheel(3., at, Mods::SHIFT);
        assert_eq!(said(&mut up), [notch(MouseKind::ScrollUp)]);
    }

    #[test]
    fn focus_is_told_once_per_change() {
        let (mut st, mut up) = state((40, 10));
        st.set_focus(false);
        assert!(said(&mut up).is_empty());
        st.set_focus(true);
        st.set_focus(true);
        st.set_focus(false);
        assert_eq!(said(&mut up), [TermUp::Focus(true), TermUp::Focus(false)]);
    }

    #[test]
    fn exit_is_noted_and_the_bell_is_not() {
        let (mut st, _up) = state((40, 10));
        st.down(TermDown::Bell);
        assert_eq!(st.exited, None);
        st.down(TermDown::Exited { status: 3 });
        assert_eq!(st.exited, Some(3));
    }
}
