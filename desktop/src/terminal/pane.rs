//! A terminal pane: the view entity that owns a [`RowCache`] (the one the real client uses, from
//! `femme-term`), applies the frames a synthetic holder streams to it and renders them with
//! [`TerminalElement`].
//!
//! Threading: the source runs on tokio worker threads and sends owned [`Frame`]s over a channel
//! bounded at the holder's unacked window; the pump task moves them into the pane's inbox and asks
//! for a redraw. Frames are applied, and acked, when the pane is drawn (`render`): a pane nobody
//! draws (hidden window, closed drawer) stops acking, so the holder stops at its window and merges
//! instead of the UI applying frames nobody sees. Nothing on the UI thread awaits the source.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::time::Duration;

use femme_proto::term::{Frame, TermUp};
use femme_proto::{Id, PaneId};
use femme_term::client::{ApplyError, RowCache};
use gpui::{
    div, App, Context, FocusHandle, Focusable, InteractiveElement as _, IntoElement, MouseButton,
    ParentElement as _, Pixels, Render, SharedString, Styled as _, Task, Window,
};
use tokio::sync::mpsc;

use super::element::TerminalElement;
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
    /// Grid size last requested from the source.
    pub wanted: (u16, u16),
    /// Frames applied.
    pub applied: u64,
    /// Times the stream was re-opened because a frame did not apply (a rev gap or a holder restart).
    pub resyncs: u64,
    /// Frames received and not applied yet: the holder keeps at most [`WINDOW`] unacked.
    inbox: VecDeque<Frame>,
    /// An `Open` is out and its snapshot has not come yet: deltas sent before it are dropped.
    resyncing: bool,
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
            wanted: initial,
            applied: 0,
            resyncs: 0,
            inbox: VecDeque::new(),
            resyncing: false,
            pane,
            up,
        };
        st.open();
        st
    }

    /// Asks the source for a `cols x rows` grid if that differs from what was last asked.
    pub fn want_size(&mut self, cols: u16, rows: u16) {
        if (cols, rows) != self.wanted {
            self.wanted = (cols, rows);
            let _ = self.up.send(TermUp::Resize { cols, rows });
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

    /// Applies one frame and acks it; a frame that does not apply re-opens the stream.
    fn apply(&mut self, f: &Frame) {
        if self.resyncing && f.base_rev != 0 {
            return; // sent before our `Open` got there: the snapshot is on its way
        }
        // ponytail: `RowCache::apply` clones the rows out of a borrowed frame; an owned
        // `apply(Frame)` in femme-term would move them in (follow-up for its owner).
        match self.cache.apply(f) {
            Ok(()) => {
                self.resyncing = false;
                self.applied += 1;
                let _ = self.up.send(TermUp::Ack {
                    rev: self.cache.rev,
                });
            }
            Err(e) => {
                self.resyncs += 1;
                if e == ApplyError::EpochChanged {
                    self.cache = RowCache::new();
                }
                self.open();
            }
        }
    }

    /// Says `Open` with what the cache holds; the answer is a snapshot.
    fn open(&mut self) {
        self.resyncing = true;
        let _ = self.up.send(TermUp::Open {
            pane: self.pane,
            epoch: self.cache.epoch,
            have_rev: self.cache.rev,
            // ponytail: the process id; a real client keeps a random id across restarts.
            client_instance: u128::from(std::process::id()),
        });
    }
}

/// Spawns `src` as a holder on the side runtime, ticking at `hz` with its first tick `phase` (0..1) of
/// a period after the start, so several sources can be staggered instead of all landing in the same
/// frame. Returns the frame stream (bounded at the holder's unacked window) and the channel for what
/// the client says upward: `Open` first (nothing is sent before it), then `Ack`s and `Resize`s (the
/// next frame after a resize is a full snapshot).
pub fn spawn_source(
    rt: &tokio::runtime::Handle,
    src: Box<dyn FrameSource>,
    hz: u32,
    phase: f32,
) -> (mpsc::Receiver<Frame>, mpsc::UnboundedSender<TermUp>) {
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
                if tx.send(f).await.is_err() {
                    break;
                }
            }
        }
    });
    (rx, up_tx)
}

/// One terminal pane view.
// ponytail: display only; keys and mouse go nowhere until the net layer can carry them (M2), and the
// source is synthetic: a real pane would hand its frame channel here instead of `spawn_source`.
pub struct TerminalPane {
    state: Rc<RefCell<PaneState>>,
    focus: FocusHandle,
    label: SharedString,
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
        let (mut rx, up) = spawn_source(rt, kind.build(initial.0, initial.1, seed), hz, phase);
        let pane = PaneId {
            sid: Id(1),
            no: id as u16,
        };
        let state = Rc::new(RefCell::new(PaneState::new(pane, up, initial)));
        let pump_state = state.clone();
        let pump = cx.spawn(async move |this, cx| {
            while let Some(f) = rx.recv().await {
                let alive = this
                    .update(cx, |_, cx| {
                        pump_state.borrow_mut().push(f);
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
            label: format!("{} {}", id + 1, kind.name()).into(),
            _pump: pump,
        }
    }

    /// Short label for the drawer tab.
    pub fn label(&self) -> SharedString {
        self.label.clone()
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
}

impl Focusable for TerminalPane {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for TerminalPane {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.state.borrow_mut().drain();
        div()
            .size_full()
            .track_focus(&self.focus)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| window.focus(&this.focus, cx)),
            )
            .child(TerminalElement::new(self.state.clone(), self.focus.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// A pane wired to a holder on the same thread.
    struct Rig {
        st: PaneState,
        up: mpsc::UnboundedReceiver<TermUp>,
        holder: Holder,
    }

    impl Rig {
        /// A pane attached to a holder running `src`, drawn once (the snapshot is applied).
        fn new(src: Box<dyn FrameSource>) -> Self {
            let (tx, up) = mpsc::unbounded_channel();
            let mut rig = Rig {
                st: PaneState::new(pane(), tx, (40, 10)),
                up,
                holder: Holder::new(src),
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
    fn pane_resets_and_reopens_after_a_holder_restart() {
        let mut rig = Rig::new(Box::new(ShellScroll::new(40, 10, 1)));
        for _ in 0..3 {
            rig.tick(false);
        }
        let epoch = rig.st.cache.epoch;
        rig.holder.restart();
        rig.tick(false); // a frame from another epoch
        assert_eq!(rig.st.resyncs, 1);
        assert_ne!(
            rig.st.cache.epoch, epoch,
            "the new holder's snapshot was applied"
        );
        rig.tick(false);
        assert_eq!(rig.st.resyncs, 1);
    }

    #[tokio::test]
    async fn source_answers_open_and_resizes() {
        let rt = tokio::runtime::Handle::current();
        let (mut frames, up) = spawn_source(&rt, SourceKind::Fire.build(20, 8, 1), 200, 0.0);
        up.send(TermUp::Open {
            pane: pane(),
            epoch: 0,
            have_rev: 0,
            client_instance: 1,
        })
        .unwrap();
        let first = frames.recv().await.unwrap();
        assert_eq!((first.cols, first.rows, first.base_rev), (20, 8, 0));
        up.send(TermUp::Resize { cols: 30, rows: 12 }).unwrap();
        let snap = loop {
            let f = frames.recv().await.unwrap();
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
}
