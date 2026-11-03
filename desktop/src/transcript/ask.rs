//! What the user has done in a question row that waits for an answer, and how the answer is sent.
//! The row's own look is in `rows.rs`; the question itself is read by `question.rs`.

use femme_proto::rpc::{Answer, Req};
use femme_proto::state::Question;
use gpui::{AppContext as _, Context, Entity, Subscription, Window};
use gpui_component::input::{InputEvent, InputState};

use super::live::Transcript;
use super::question;

/// The state of one open question, by the `seq` of its item.
pub struct Ask {
    /// Which options of a multiple-choice question are ticked.
    pub picks: Vec<bool>,
    /// The field for a free-text answer, when the question takes one.
    pub input: Option<Entity<InputState>>,
    /// An answer was sent and the daemon has not yet changed the item.
    pub sending: bool,
    /// Enter in the field sends the answer.
    _enter: Option<Subscription>,
}

impl Transcript {
    /// The state of question `seq`, made when it is first seen open. The field needs a window,
    /// which only the row builder has.
    pub(super) fn ask_for(
        &mut self,
        seq: u64,
        q: &Question,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.asks.contains_key(&seq) {
            return;
        }
        let (input, enter) = if q.free_text {
            let input = cx.new(|cx| InputState::new(window, cx).placeholder("or type an answer"));
            let enter =
                cx.subscribe_in(&input, window, move |this, _, event: &InputEvent, _, cx| {
                    if matches!(event, InputEvent::PressEnter { .. }) {
                        this.submit_text(seq, cx);
                    }
                });
            (Some(input), Some(enter))
        } else {
            (None, None)
        };
        self.asks.insert(
            seq,
            Ask {
                picks: vec![false; q.options.len()],
                input,
                sending: false,
                _enter: enter,
            },
        );
    }

    /// Ticks or unticks option `i` of a multiple-choice question.
    pub(super) fn pick(&mut self, seq: u64, i: usize, cx: &mut Context<Self>) {
        if let Some(on) = self.asks.get_mut(&seq).and_then(|a| a.picks.get_mut(i)) {
            *on = !*on;
            cx.notify();
        }
    }

    /// Sends the ticked options of a multiple-choice question; false when none is ticked.
    pub(super) fn submit_picks(&mut self, seq: u64, cx: &mut Context<Self>) -> bool {
        let Some(ask) = self.asks.get(&seq) else {
            return false;
        };
        if !ask.picks.iter().any(|on| *on) {
            return false;
        }
        let answer = question::picks(&ask.picks);
        self.answer(seq, answer, cx);
        true
    }

    /// Sends what is typed in the question's field, if anything is.
    pub(super) fn submit_text(&mut self, seq: u64, cx: &mut Context<Self>) {
        let Some(input) = self.asks.get(&seq).and_then(|a| a.input.clone()) else {
            return;
        };
        let text = input.read(cx).value().trim().to_owned();
        if !text.is_empty() {
            self.answer(seq, Answer::Text(text), cx);
        }
    }

    /// Answers question `seq`. The daemon turns the item into an answered one; until it does the
    /// row says "sending".
    pub(super) fn answer(&mut self, seq: u64, answer: Answer, cx: &mut Context<Self>) {
        let Some(sid) = self.items.sid() else {
            return;
        };
        let Some(request_id) = self
            .items
            .get(seq)
            .and_then(|e| match self.items.parsed(e) {
                super::body::Parsed::Question(q) => Some(q.q.request_id.clone()),
                _ => None,
            })
        else {
            return;
        };
        match self.asks.get_mut(&seq) {
            Some(ask) if !ask.sending => ask.sending = true,
            _ => return,
        }
        self.request(
            Req::AnswerQuestion {
                sid,
                request_id,
                answer,
            },
            cx,
            move |this, res, cx| {
                // Sent: the row keeps saying so until the daemon changes the item. Not sent: the
                // buttons come back.
                if let Err(e) = res {
                    if let Some(ask) = this.asks.get_mut(&seq) {
                        ask.sending = false;
                    }
                    this.note = Some(format!("the answer was not sent: {e}"));
                }
                cx.notify();
            },
        );
        cx.notify();
    }
}
