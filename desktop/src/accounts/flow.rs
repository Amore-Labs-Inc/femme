//! What the card asks the box and what each button does. Every request goes out through
//! [`AccountsOverlay::call`] and its answer comes back to the overlay, which is the only owner of
//! state. The card reads everything when it opens and again every [`REFRESH`] seconds while it is
//! open (the board's own page asks every 10 s as well), one request at a time so that the failure a
//! request leaves in `Net::error` is its own.

use std::time::Duration;

use femme_proto::rpc::{Req, Resp};
use gpui::{ClipboardItem, Context, Entity};
use gpui_component::input::InputState;

use super::model::{self, Caps};
use super::source::Source;
use super::{api, AccountsOverlay, CapStatus, Load, Notice};

/// Seconds between two reads while the card is open.
const REFRESH: u32 = 10;
/// Seconds between two reads of the keys while a GitHub sign-in waits for its code to be entered.
const POLL_GITHUB: u32 = 3;
/// How long a cap box rests after a keystroke before its value is sent.
const CAP_DEBOUNCE: Duration = Duration::from_millis(500);
/// How often the closed card re-reads the accounts for the menu's sign-in dot.
const WATCH: Duration = Duration::from_secs(10 * 60);

/// One thing the card reads from the box.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Step {
    /// `GET /api/accounts`.
    Accounts,
    /// `GET /api/keys`.
    Keys,
    /// `Req::Caps`.
    Caps,
    /// `GET /api/meta`.
    Meta,
}

impl Step {
    fn request(self) -> Req {
        match self {
            Step::Accounts => api::accounts(),
            Step::Keys => api::keys(),
            Step::Caps => api::caps(),
            Step::Meta => api::meta(),
        }
    }
}

/// Everything the card shows, in the order it needs it.
pub(super) const ALL: [Step; 4] = [Step::Accounts, Step::Keys, Step::Caps, Step::Meta];
const KEYS: [Step; 1] = [Step::Keys];
const ACCOUNTS: [Step; 1] = [Step::Accounts];
const CAPS: [Step; 1] = [Step::Caps];

/// The body of a board call that went through, else why it did not: the board's own words for an
/// error status, the daemon's for a request that never got an answer.
pub(super) fn board_body(answer: Result<Resp, String>) -> Result<String, String> {
    match answer {
        Ok(Resp::BoardApi { status, body }) if (200..300).contains(&status) => Ok(body),
        Ok(Resp::BoardApi { status, body }) => Err(model::error_text(status, &body)),
        Ok(_) => Err("the daemon answered something else than the board".into()),
        Err(e) => Err(model::explain(&e)),
    }
}

/// Whether a read of `step` that went through takes back `notice`: the one that says a read failed
/// goes when the accounts, which every pass starts with, come in again.
fn takes_back(notice: &Notice, step: Step) -> bool {
    notice.read && step == Step::Accounts
}

impl AccountsOverlay {
    /// Sends `req`; `done` runs on the overlay with the answer, or with why there is none.
    fn call(
        &mut self,
        req: Req,
        cx: &mut Context<Self>,
        done: impl FnOnce(&mut Self, Result<Resp, String>, &mut Context<Self>) + 'static,
    ) {
        let sent = self.source.send(req, cx);
        cx.spawn(async move |this, cx| {
            let answer = sent.await;
            this.update(cx, |t, cx| done(t, answer, cx)).ok();
        })
        .detach();
    }

    /// [`call`](Self::call) for a board route: `done` gets the body of a 2xx answer, or the words of
    /// the failure.
    fn board(
        &mut self,
        req: Req,
        cx: &mut Context<Self>,
        done: impl FnOnce(&mut Self, Result<String, String>, &mut Context<Self>) + 'static,
    ) {
        self.call(req, cx, move |t, answer, cx| {
            done(t, board_body(answer), cx)
        });
    }

    /// Whether a read of the box is on its way.
    pub(super) fn reading(&self) -> bool {
        self.reload.as_ref().is_some_and(|t| !t.is_ready())
    }

    /// Reads `steps` from the box one after the other, in place of a read that is still out.
    fn read(&mut self, steps: &'static [Step], cx: &mut Context<Self>) {
        self.reload = Some(cx.spawn(async move |this, cx| {
            for &step in steps {
                let Ok(sent) = this.update(cx, |t, cx| t.source.send(step.request(), cx)) else {
                    return;
                };
                let answer = sent.await;
                if this.update(cx, |t, cx| t.loaded(step, answer, cx)).is_err() {
                    return;
                }
            }
        }));
    }

    /// Reads the accounts now and every [`WATCH`] while the card is closed, so the menu can show a
    /// sign-in that has ended without the card ever having been opened.
    pub(super) fn watch_sign_ins(&mut self, cx: &mut Context<Self>) {
        self.watcher = Some(cx.spawn(async move |this, cx| loop {
            if this
                .update(cx, |t, cx| {
                    if !t.open && !t.busy && !t.reading() {
                        t.read(&ACCOUNTS, cx);
                    }
                })
                .is_err()
            {
                break;
            }
            cx.background_executor().timer(WATCH).await;
        }));
    }

    /// Reads everything again, unless a button's request is out and about to say it better.
    pub(super) fn refresh(&mut self, cx: &mut Context<Self>) {
        if !self.busy {
            self.read(&ALL, cx);
        }
    }

    /// An answer to a read.
    fn loaded(&mut self, step: Step, answer: Result<Resp, String>, cx: &mut Context<Self>) {
        let failed = match step {
            Step::Accounts => self
                .accounts
                .settle(board_body(answer).and_then(|b| model::parse_accounts(&b))),
            Step::Keys => self
                .keys
                .settle(board_body(answer).and_then(|b| model::parse_keys(&b))),
            Step::Caps => self.caps.settle(match answer {
                Ok(Resp::Caps(list)) => {
                    Ok(list.iter().map(|c| (c.account, Caps::from(c))).collect())
                }
                Ok(_) => Err("the daemon answered something else than the caps".into()),
                Err(e) => Err(model::explain(&e)),
            }),
            Step::Meta => {
                // The footer says less without it: not worth a notice.
                if let Ok(body) = board_body(answer) {
                    self.meta = model::parse_meta(&body);
                }
                None
            }
        };
        if let Some(why) = failed {
            self.notice = Some(Notice::lost(why));
        } else if matches!(self.accounts, Load::Ready(_))
            && self.notice.as_ref().is_some_and(|n| takes_back(n, step))
        {
            // The box answers again: what it failed to say is no news any more.
            self.notice = None;
        }
        cx.notify();
    }

    /// One second of the open card: asks again when it is time. Whether it goes on.
    pub(super) fn tick(&mut self, cx: &mut Context<Self>) -> bool {
        if !self.open {
            return false;
        }
        self.ticks += 1;
        if self.busy || self.reading() {
            return true;
        }
        if self.ticks.is_multiple_of(REFRESH) {
            self.read(&ALL, cx);
        } else if self.ticks.is_multiple_of(POLL_GITHUB) && self.github_waiting() {
            self.read(&KEYS, cx);
        }
        true
    }

    /// Whether a GitHub sign-in waits for its code to be entered: the key shows who pushes once it is.
    fn github_waiting(&self) -> bool {
        matches!(&self.keys, Load::Ready(keys) if keys.iter().any(|k| k.github.is_some()))
    }

    /// Starts a button's request, unless another is out: nothing else is asked while it is.
    fn begin(&mut self, cx: &mut Context<Self>) -> bool {
        if self.busy {
            return false;
        }
        self.busy = true;
        self.notice = None;
        self.reload = None;
        cx.notify();
        true
    }

    /// The answer to an account action: the account list as it is now, or why it was refused (and
    /// then the list as it is, as the page does).
    fn acted(&mut self, body: Result<String, String>, cx: &mut Context<Self>) {
        self.busy = false;
        match body.and_then(|b| model::parse_accounts(&b)) {
            Ok(doc) => self.accounts = Load::Ready(doc),
            Err(why) => {
                self.notice = Some(Notice::bad(why));
                self.read(&ALL, cx);
            }
        }
        cx.notify();
    }

    /// The answer to a key action that returns the key list.
    fn keys_acted(&mut self, body: Result<String, String>, cx: &mut Context<Self>) {
        self.busy = false;
        match body.and_then(|b| model::parse_keys(&b)) {
            Ok(keys) => self.keys = Load::Ready(keys),
            Err(why) => self.notice = Some(Notice::bad(why)),
        }
        cx.notify();
    }

    /// "switch": the box moves to account `number`.
    pub(super) fn switch(&mut self, number: u32, cx: &mut Context<Self>) {
        if self.begin(cx) {
            self.board(api::switch(number), cx, |t, body, cx| t.acted(body, cx));
        }
    }

    /// "+ add": starts a Claude sign-in on the box and opens its link (a daemon's, not a fixture's).
    pub(super) fn login_start(&mut self, cx: &mut Context<Self>) {
        if !self.begin(cx) {
            return;
        }
        self.board(api::login_start(), cx, |t, body, cx| {
            t.acted(body, cx);
            let link = match &t.accounts {
                Load::Ready(doc) => doc.login.clone(),
                _ => None,
            };
            if let Some(url) = link {
                t.todo.focus_code = true;
                if matches!(t.source, Source::Daemon(_)) {
                    cx.open_url(&url);
                }
            }
        });
    }

    /// "Add account": hands the sign-in the code the page showed.
    pub(super) fn login_code(&mut self, cx: &mut Context<Self>) {
        let Some(code) = self.fields.as_ref().map(|f| f.code.read(cx).value()) else {
            return;
        };
        if code.trim().is_empty() {
            self.notice = Some(Notice::bad("paste the code the sign-in page shows"));
            cx.notify();
            return;
        }
        if !self.begin(cx) {
            return;
        }
        self.board(api::login_code(&code), cx, |t, body, cx| {
            if body.is_ok() {
                t.todo.clear_code = true;
            }
            t.acted(body, cx);
        });
    }

    /// "Cancel": drops the sign-in in progress.
    pub(super) fn login_cancel(&mut self, cx: &mut Context<Self>) {
        if self.begin(cx) {
            self.todo.clear_code = true;
            self.board(api::login_cancel(), cx, |t, body, cx| t.acted(body, cx));
        }
    }

    /// "+ key": makes a key from the name (and GitHub token) in the form.
    pub(super) fn key_add(&mut self, cx: &mut Context<Self>) {
        let Some((name, token)) = self
            .fields
            .as_ref()
            .map(|f| (f.key_name.read(cx).value(), f.key_token.read(cx).value()))
        else {
            return;
        };
        if !api::valid_key_name(name.trim()) {
            self.notice = Some(Notice::bad(api::NAME_RULE));
            cx.notify();
            return;
        }
        if !self.begin(cx) {
            return;
        }
        self.board(api::key_add(&name, &token), cx, |t, body, cx| {
            t.busy = false;
            match body.and_then(|b| model::parse_new_key(&b)) {
                Ok(made) => {
                    t.keys = Load::Ready(made.keys);
                    t.secret = Some((made.name, made.key));
                    t.todo.clear_key = true;
                }
                Err(why) => t.notice = Some(Notice::bad(why)),
            }
            cx.notify();
        });
    }

    /// "revoke": asks for a second click first, as the page asks for a confirmation.
    pub(super) fn ask_revoke(&mut self, name: String, cx: &mut Context<Self>) {
        self.revoking = Some(name);
        cx.notify();
    }

    /// "keep": takes the question back.
    pub(super) fn keep(&mut self, cx: &mut Context<Self>) {
        self.revoking = None;
        cx.notify();
    }

    /// The second click: the key stops working.
    pub(super) fn revoke(&mut self, name: &str, cx: &mut Context<Self>) {
        self.revoking = None;
        if self.begin(cx) {
            self.board(api::key_revoke(name), cx, |t, body, cx| {
                t.keys_acted(body, cx);
            });
        }
    }

    /// "connect GitHub" (`start`) or its "cancel": the key's GitHub sign-in.
    pub(super) fn github(&mut self, name: &str, start: bool, cx: &mut Context<Self>) {
        if !self.begin(cx) {
            return;
        }
        let req = if start {
            api::github_start(name)
        } else {
            api::github_cancel(name)
        };
        self.board(req, cx, |t, body, cx| t.keys_acted(body, cx));
    }

    /// "copy": the new key's secret goes to the clipboard.
    pub(super) fn copy_secret(&mut self, cx: &mut Context<Self>) {
        if let Some((_, key)) = &self.secret {
            cx.write_to_clipboard(ClipboardItem::new_string(key.clone()));
            self.notice = Some(Notice::ok("copied"));
            cx.notify();
        }
    }

    /// A cap box changed: its value is sent once the keys have rested for [`CAP_DEBOUNCE`].
    pub(super) fn cap_edited(&mut self, number: u32, cx: &mut Context<Self>) {
        let Some(row) = self.rows.get_mut(&number) else {
            return;
        };
        row.status = CapStatus::Idle;
        row.debounce = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(CAP_DEBOUNCE).await;
            this.update(cx, |t, cx| t.cap_commit(number, cx)).ok();
        }));
        cx.notify();
    }

    /// Sends what the two cap boxes of account `number` hold, then reads the caps back. A value out
    /// of range is not sent: the box says so instead.
    pub(super) fn cap_commit(&mut self, number: u32, cx: &mut Context<Self>) {
        let Some(row) = self.rows.get_mut(&number) else {
            return;
        };
        row.debounce = None;
        if row.saving {
            // The last one is still out: the answers could cross.
            self.cap_edited(number, cx);
            return;
        }
        let read = |s: &Entity<InputState>| model::parse_cap(&s.read(cx).value());
        let (five, week) = match (read(&row.five), read(&row.week)) {
            (Ok(five), Ok(week)) => (five, week),
            (Err(why), _) | (_, Err(why)) => {
                row.status = CapStatus::Bad(why);
                cx.notify();
                return;
            }
        };
        row.saving = true;
        row.status = CapStatus::Saving;
        cx.notify();
        self.call(
            api::set_cap(number, five, week),
            cx,
            move |t, answer, cx| {
                let stored = matches!(answer, Ok(Resp::Ok));
                if let Some(row) = t.rows.get_mut(&number) {
                    row.saving = false;
                    row.status = match answer {
                        Ok(Resp::Ok) => CapStatus::Saved,
                        Ok(_) => CapStatus::Bad("the daemon answered something else".into()),
                        Err(why) => CapStatus::Bad(model::explain(&why)),
                    };
                }
                if stored {
                    t.read(&CAPS, cx);
                }
                cx.notify();
            },
        );
    }

    /// Sends every cap value that is still resting, now (the card is closing).
    pub(super) fn flush_caps(&mut self, cx: &mut Context<Self>) {
        let waiting: Vec<u32> = self
            .rows
            .iter()
            .filter(|(_, r)| r.debounce.is_some())
            .map(|(n, _)| *n)
            .collect();
        for number in waiting {
            self.cap_commit(number, cx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_reads_notice_goes_when_the_accounts_come_in_again() {
        assert!(takes_back(
            &Notice::lost("the board is not answering"),
            Step::Accounts
        ));
        assert!(!takes_back(
            &Notice::lost("the board is not answering"),
            Step::Keys
        ));
        // A refusal of a button stays until the next button.
        assert!(!takes_back(
            &Notice::bad("a key with that name exists"),
            Step::Accounts
        ));
        assert!(!takes_back(&Notice::ok("copied"), Step::Accounts));
    }

    fn board(status: u16, body: &str) -> Result<Resp, String> {
        Ok(Resp::BoardApi {
            status,
            body: body.into(),
        })
    }

    #[test]
    fn a_board_answer_is_its_body_or_the_boards_words() {
        assert_eq!(board_body(board(200, "[]")), Ok("[]".into()));
        assert_eq!(board_body(board(201, "{}")), Ok("{}".into()));
        assert_eq!(
            board_body(board(409, r#"{"error": "a key with that name exists"}"#)),
            Err("a key with that name exists".into())
        );
        assert_eq!(board_body(board(502, "")), Err("HTTP 502".into()));
        assert_eq!(board_body(board(199, "x")), Err("x".into()));
    }

    #[test]
    fn a_request_that_fails_says_so_plainly() {
        assert_eq!(
            board_body(Err("Unsupported: not served yet".into())),
            Err("daemon too old for accounts (Unsupported: not served yet)".into())
        );
        assert_eq!(
            board_body(Err("Timeout: no answer".into())),
            Err("Timeout: no answer".into())
        );
        assert!(board_body(Ok(Resp::Ok)).is_err());
    }
}
