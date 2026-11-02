//! The virtualised transcript list.
//!
//! `list()` keeps only the rows near the viewport alive and measures the rest from a uniform height
//! hint, so 50 000 rows cost the same per frame as 50. The last row streams: a task on the side
//! tokio runtime stands in for the network and sends bursts of words, the view hands them to the
//! [`Pacer`], and the pacer is ticked from gpui's frame scheduler (`on_next_frame`), one tick per
//! frame, only while words are waiting: caught up, nothing is scheduled. A message ends after
//! [`MESSAGE_WORDS`] words and the next one starts as a new row, so no row grows without bound.

use std::time::{Duration, Instant};

use gpui::prelude::FluentBuilder as _;
use gpui::{
    div, list, px, relative, AnyElement, App, Context, FocusHandle, Focusable, FollowMode,
    InteractiveElement as _, IntoElement, KeyDownEvent, ListAlignment, ListState, MouseButton,
    ParentElement as _, Render, Styled as _, Task, Window,
};
use gpui_component::scroll::Scrollbar;
use tokio::sync::mpsc;

use super::model::{fake_rows, TranscriptRow, WordStream};
use super::pacer::Pacer;
use crate::rng::Rng;
use crate::theme::{BODY_SIZE, FONT_MONO, FONT_PIXEL, MUTED, PAPER, PR, WORK};
use crate::ui::{button, card, dialog, meta, st, tb};

/// Words in one streamed message before it ends and the next one starts.
pub const MESSAGE_WORDS: usize = 200;

/// Longest frame gap the pacer is told about: a window that was not drawn for a while (hidden,
/// occluded) resumes at pace instead of revealing what arrived meanwhile in one frame.
const MAX_DT: Duration = Duration::from_millis(100);

/// What the stand-in for the network sends.
#[derive(Debug, PartialEq)]
pub enum Chunk {
    /// A burst of words, each after a separator (the first burst of a new message has none).
    Words(String),
    /// The message is complete.
    End,
}

/// The synthetic network: a burst of 4-24 words every 0.45-1.25 s, as SSE chunks would arrive, and
/// the end of the message once it holds [`MESSAGE_WORDS`] words.
struct Source {
    words: WordStream,
    rng: Rng,
    /// Words sent in the current message.
    sent: usize,
    /// The next burst opens a message of its own. The very first one continues the transcript's
    /// last row instead, so it keeps its leading separator.
    fresh: bool,
}

impl Source {
    fn new(seed: u64) -> Self {
        Self {
            words: WordStream::new(seed),
            rng: Rng::new(seed ^ 0x5EED),
            sent: 0,
            fresh: false,
        }
    }

    /// The pause before the next burst, the burst, and whether it completes the message.
    fn next(&mut self) -> (Duration, String, bool) {
        let pause = Duration::from_millis(u64::from(self.rng.range(450, 1250)));
        let n = self.rng.range(4, 24) as usize;
        let mut burst = self.words.next_burst(n);
        if std::mem::take(&mut self.fresh) {
            burst = burst.trim_start().to_owned();
        }
        self.sent += n;
        let end = self.sent >= MESSAGE_WORDS;
        if end {
            (self.sent, self.fresh) = (0, true);
        }
        (pause, burst, end)
    }
}

/// Spawns the streaming source on the side runtime (a timer is fine here: it stands in for the
/// network). The channel closes when the receiver is dropped.
pub fn spawn_stream(rt: &tokio::runtime::Handle, seed: u64) -> mpsc::UnboundedReceiver<Chunk> {
    let (tx, rx) = mpsc::unbounded_channel();
    rt.spawn(async move {
        let mut source = Source::new(seed);
        loop {
            let (pause, words, end) = source.next();
            tokio::time::sleep(pause).await;
            if tx.send(Chunk::Words(words)).is_err() || (end && tx.send(Chunk::End).is_err()) {
                break;
            }
        }
    });
    rx
}

/// The pacer of a new message: 12-20 words/s.
fn pacer() -> Pacer {
    Pacer::new(12.0, 20.0)
}

/// The message being revealed, and the frame loop that drives its pacer.
struct Live {
    pacer: Pacer,
    /// Bytes of `pacer.revealed()` already handed to the row.
    shown: usize,
    /// A frame callback is pending, so the loop is running.
    armed: bool,
    /// When the loop handled its previous frame.
    last: Option<Instant>,
}

impl Live {
    fn new() -> Self {
        Self {
            pacer: pacer(),
            shown: 0,
            armed: false,
            last: None,
        }
    }

    /// What the pacer revealed since the last call.
    fn unseen(&mut self) -> &str {
        let revealed = self.pacer.revealed();
        let new = &revealed[self.shown..];
        self.shown = revealed.len();
        new
    }

    /// Starts the next message. A pending frame callback of the loop stays valid: it finds the
    /// fresh pacer caught up and stops.
    fn next_message(&mut self) {
        self.pacer = pacer();
        self.shown = 0;
    }

    /// Takes in arrived words. True when the frame loop has to be started: words are waiting and
    /// it is not already running.
    fn push(&mut self, words: &str) -> bool {
        self.pacer.push(words);
        let start = !self.armed && !self.pacer.is_caught_up();
        if start {
            self.armed = true;
            self.last = None;
        }
        start
    }

    /// One frame of the loop at `now`: ticks the pacer by the time since the previous frame (none
    /// for the first one, at most [`MAX_DT`]). Returns the words revealed and whether another
    /// frame is wanted; if not, the loop is stopped.
    fn frame(&mut self, now: Instant) -> (usize, bool) {
        let dt = self.last.replace(now).map_or(Duration::ZERO, |t| {
            now.saturating_duration_since(t).min(MAX_DT)
        });
        let revealed = self.pacer.tick(dt);
        let again = !self.pacer.is_caught_up();
        if !again {
            self.armed = false;
            self.last = None;
        }
        (revealed, again)
    }
}

/// The transcript view: a list of [`TranscriptRow`]s with a scrollbar, pinned to the tail.
// ponytail: messages are rows of one view, not an entity each (plan §4): the split pays when
// markdown parsing lands, so that only the live message is reparsed.
pub struct TranscriptView {
    rows: Vec<TranscriptRow>,
    list: ListState,
    focus: FocusHandle,
    live: Live,
    _pump: Option<Task<()>>,
}

impl TranscriptView {
    /// A transcript of `n` fake rows whose last row streams. `n == 0` gives an empty transcript.
    pub fn new(
        n: usize,
        seed: u64,
        rt: &tokio::runtime::Handle,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let rows = fake_rows(n, seed);
        // Uniform hint so the scrollbar thumb is sized from the first frame without measuring 50k rows.
        let list = ListState::new(rows.len(), ListAlignment::Bottom, px(600.))
            .with_uniform_item_height(px(84.));
        list.set_follow_mode(FollowMode::Tail);
        let pump = (!rows.is_empty()).then(|| {
            let mut rx = spawn_stream(rt, seed.wrapping_add(1));
            cx.spawn_in(window, async move |this, cx| {
                while let Some(chunk) = rx.recv().await {
                    if this
                        .update_in(cx, |this, window, cx| this.receive(chunk, window, cx))
                        .is_err()
                    {
                        break;
                    }
                }
            })
        });
        Self {
            rows,
            list,
            focus: cx.focus_handle(),
            live: Live::new(),
            _pump: pump,
        }
    }

    fn receive(&mut self, chunk: Chunk, window: &mut Window, cx: &mut Context<Self>) {
        match chunk {
            Chunk::Words(words) => {
                if self.live.push(&words) {
                    cx.on_next_frame(window, Self::frame);
                }
            }
            Chunk::End => self.end_message(cx),
        }
    }

    /// A frame of the reveal loop: shows what the pacer let out and asks for the next frame only
    /// if words are still waiting.
    fn frame(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (revealed, again) = self.live.frame(Instant::now());
        if revealed > 0 {
            self.sync();
            cx.notify();
        }
        if again {
            cx.on_next_frame(window, Self::frame);
        }
    }

    /// Brings the last row up to what the pacer revealed; the list re-measures it.
    fn sync(&mut self) {
        let last = self.rows.len().saturating_sub(1);
        if let Some(TranscriptRow::Assistant { text, .. }) = self.rows.last_mut() {
            text.push_str(self.live.unseen());
            self.list.splice(last..last + 1, 1);
        }
    }

    /// The message is complete: shows all of it and starts the next one as a new row.
    // ponytail: the turn's last words appear at once (`Pacer::finish`); a backlog is a few words.
    fn end_message(&mut self, cx: &mut Context<Self>) {
        self.live.pacer.finish();
        self.sync();
        self.live.next_message();
        let n = self.rows.len();
        let seq = self.rows.last().map_or(1, |r| r.seq() + 1);
        self.rows.push(TranscriptRow::Assistant {
            seq,
            text: String::new(),
        });
        self.list.splice(n..n, 1);
        cx.notify();
    }

    fn row(&mut self, ix: usize, _: &mut Window, _: &mut Context<Self>) -> AnyElement {
        let Some(row) = self.rows.get(ix) else {
            return div().into_any_element();
        };
        let live = ix + 1 == self.rows.len();
        match row {
            TranscriptRow::User { text, .. } => div()
                .w_full()
                .px(px(12.))
                .py(px(5.))
                .child(
                    card()
                        .flex()
                        .flex_col()
                        .gap(px(4.))
                        .child(
                            div()
                                .font_family(FONT_PIXEL)
                                .text_size(px(18.))
                                .line_height(relative(1.))
                                .text_color(MUTED)
                                .child("you"),
                        )
                        .child(
                            div()
                                .text_size(BODY_SIZE)
                                .line_height(relative(1.4))
                                .child(text.clone()),
                        ),
                )
                .into_any_element(),
            // ponytail: pulldown-cmark + mdstream in M3 (assistant text is plain here: no code blocks, lists or links)
            TranscriptRow::Assistant { text, .. } => div()
                .w_full()
                .flex()
                .flex_col()
                .gap(px(4.))
                .px(px(16.))
                .py(px(6.))
                .when(live, |d| d.child(div().flex().child(st("streaming", WORK))))
                .child(
                    div()
                        .text_size(BODY_SIZE)
                        .line_height(relative(1.45))
                        .child(text.clone()),
                )
                .into_any_element(),
            row @ TranscriptRow::ToolFold { summary, .. } => div()
                .w_full()
                .px(px(12.))
                .py(px(3.))
                .flex()
                .items_center()
                .gap(px(8.))
                .child(st("tools", PR))
                .child(
                    div()
                        .font_family(FONT_MONO)
                        .text_size(px(13.))
                        .child(row.fold_label().unwrap_or_default()),
                )
                .child(meta(summary.clone()))
                .into_any_element(),
            TranscriptRow::Question {
                prompt, options, ..
            } => {
                div()
                    .w_full()
                    .px(px(12.))
                    .py(px(6.))
                    .child(
                        dialog().flex().flex_col().child(tb("question")).child(
                            div()
                                .p(px(10.))
                                .flex()
                                .flex_col()
                                .gap(px(8.))
                                .child(
                                    div()
                                        .text_size(BODY_SIZE)
                                        .line_height(relative(1.4))
                                        .child(prompt.clone()),
                                )
                                .child(div().flex().flex_wrap().gap(px(6.)).children(
                                    options.iter().enumerate().map(|(i, o)| {
                                        button(("opt", i), format!("{}. {o}", i + 1))
                                    }),
                                )),
                        ),
                    )
                    .into_any_element()
            }
        }
    }

    fn on_key(&mut self, e: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let page = self.list.viewport_bounds().size.height * 0.9;
        match e.keystroke.key.as_str() {
            "pageup" => self.list.scroll_by(-page),
            "pagedown" => self.list.scroll_by(page),
            "end" => self.list.scroll_to_end(),
            _ => return,
        }
        window.refresh();
        cx.notify();
    }
}

impl Focusable for TranscriptView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for TranscriptView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .relative()
            .bg(PAPER)
            .track_focus(&self.focus)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| window.focus(&this.focus, cx)),
            )
            .on_key_down(cx.listener(Self::on_key))
            .child(
                list(
                    self.list.clone(),
                    cx.processor(|this, ix, window, cx| this.row(ix, window, cx)),
                )
                .size_full(),
            )
            .child(Scrollbar::vertical(&self.list))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_ends_the_message_every_n_words() {
        let mut source = Source::new(3);
        let (mut words, mut messages) = (0, 0);
        for _ in 0..400 {
            let (pause, burst, end) = source.next();
            assert!(
                (450..=1250).contains(&(pause.as_millis() as u64)),
                "{pause:?}"
            );
            if words == 0 {
                // The first message continues the transcript's last row; later ones are rows of their own.
                assert_eq!(
                    burst.starts_with(char::is_whitespace),
                    messages == 0,
                    "{burst:?}"
                );
            }
            let n = burst.split_whitespace().count();
            assert!((4..=24).contains(&n), "{n} words in a burst");
            words += n;
            assert_eq!(
                end,
                words >= MESSAGE_WORDS,
                "the message ends once it holds {MESSAGE_WORDS} words"
            );
            if end {
                assert!(words < MESSAGE_WORDS + 24, "{words} words");
                (words, messages) = (0, messages + 1);
            }
        }
        assert!(messages > 10, "{messages} messages");
    }

    #[test]
    fn reveal_loop_runs_only_while_words_are_pending() {
        let mut live = Live::new();
        let mut now = Instant::now();
        assert!(
            !live.push("one"),
            "a word that may continue is not pending: nothing to schedule"
        );
        assert!(
            live.push(" two three"),
            "complete words are pending: start the loop"
        );
        assert!(!live.push(" four"), "the loop is running: no second one");
        let mut frames = 0;
        loop {
            now += Duration::from_millis(16);
            frames += 1;
            assert!(frames < 1_000, "the loop never stopped");
            if !live.frame(now).1 {
                break;
            }
        }
        assert_eq!(live.pacer.revealed(), "one two three");
        assert!(!live.armed, "caught up: no frame is scheduled");
        assert!(live.push(" five six"), "the next words start it again");
    }

    #[test]
    fn a_long_gap_between_frames_does_not_dump_the_backlog() {
        let mut live = Live::new();
        let now = Instant::now();
        live.push(&"word ".repeat(100));
        live.frame(now);
        // The window was hidden for 10 s: the pacer is told 100 ms.
        let (revealed, again) = live.frame(now + Duration::from_secs(10));
        assert!(revealed <= 2, "{revealed} words after a 10 s gap");
        assert!(again);
    }
}
