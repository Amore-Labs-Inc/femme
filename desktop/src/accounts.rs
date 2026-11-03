//! The accounts overlay (femme ▸ accounts): the board's "Me" page as a card over the window. Every
//! Claude account on the box with its 5 h / 7 d usage, the one in use, the per-account rotation caps
//! (typed by hand, sent after a 500 ms rest), AWS, and the API keys with revoke / connect GitHub /
//! add, and `+ add` for a new Claude sign-in. Owned by the accounts agent (docs/desktop-ui.md
//! §accounts).
//!
//! The data is the web app's own JSON, asked of the daemon with `Req::BoardApi` (`/api/accounts`,
//! `/api/keys`, `/api/meta`), plus `Req::Caps` / `Req::SetCap` for the caps; the card shows what
//! the board shows. A daemon that does not know those requests (`Unsupported`) makes the card say
//! "daemon too old for accounts" instead of failing. For a try-out or a screenshot without a
//! daemon, `FEMME_ACCOUNTS_FIXTURE=<file>` replaces the daemon by a JSON file: see [`source`].
//!
//! # Mounting
//!
//! The shell renders the entity in a layer as big as the window (an absolute, `inset_0` parent: the
//! card's own root is `absolute` too) while [`AccountsOverlay::is_open`], and calls
//! [`AccountsOverlay::open`] from its menu. The card takes the keyboard when it opens and gives it
//! back to whatever had it when it closes by itself (Esc, `‹`, a click outside), and emits [`Closed`]
//! then, which the shell subscribes to so that its layer goes with the card; Esc works with a text
//! box focused as well.

pub mod api;
pub mod clock;
mod flow;
pub mod model;
pub mod source;
mod view;

use std::collections::HashMap;
use std::time::Duration;

use gpui::{
    div, App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement,
    Render, Subscription, Task, Window,
};
use gpui_component::input::{InputEvent, InputState};

use crate::net::Net;
use model::{AccountList, AccountsDoc, Caps, Key, Meta};
use source::Source;
use view::cap_text;

/// Emitted when the card closes: by itself (Esc, `‹`, a click outside) or through
/// [`AccountsOverlay::close`].
pub struct Closed;

/// A part of the card while its data comes in.
#[derive(Clone, Debug, PartialEq)]
enum Load<T> {
    /// Not read yet.
    Loading,
    /// The read failed and there is nothing older to show: why.
    Failed(String),
    /// What was read.
    Ready(T),
}

impl<T> Load<T> {
    /// Takes the answer of a read. One that fails leaves what the card already shows (and hands
    /// back why) instead of wiping it, so a lost packet does not blank the card.
    fn settle(&mut self, answer: Result<T, String>) -> Option<String> {
        match answer {
            Ok(v) => {
                *self = Load::Ready(v);
                None
            }
            Err(why) if matches!(self, Load::Ready(_)) => Some(why),
            Err(why) => {
                *self = Load::Failed(why);
                None
            }
        }
    }
}

/// The line under the title bar: the last refusal (red) or confirmation (green).
struct Notice {
    text: String,
    bad: bool,
    /// It says a read failed: the next read of the accounts that goes through takes it back.
    read: bool,
}

impl Notice {
    fn bad(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            bad: true,
            read: false,
        }
    }

    fn ok(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            bad: false,
            read: false,
        }
    }

    fn lost(text: impl Into<String>) -> Self {
        Self {
            read: true,
            ..Self::bad(text)
        }
    }
}

/// Where an account's cap boxes stand.
#[derive(Clone, Debug, PartialEq)]
enum CapStatus {
    /// Nothing to say.
    Idle,
    /// The value is on its way to the box.
    Saving,
    /// The box has it.
    Saved,
    /// It was not sent or not taken: why.
    Bad(String),
}

/// The two cap boxes of one account.
struct CapRow {
    five: Entity<InputState>,
    week: Entity<InputState>,
    /// What the daemon held when the boxes were last filled from it (`None`: never).
    seen: Option<Caps>,
    /// The default shown greyed in the 5 h box.
    hint: String,
    status: CapStatus,
    /// The rest after a keystroke; the value is sent when it ends.
    debounce: Option<Task<()>>,
    /// A `SetCap` is out.
    saving: bool,
    _subs: [Subscription; 2],
}

/// The text boxes that are not an account's caps.
struct Fields {
    code: Entity<InputState>,
    key_name: Entity<InputState>,
    key_token: Entity<InputState>,
    _subs: Vec<Subscription>,
}

/// Work that needs the window, left for the next frame: the state changes where none is at hand (an
/// answer, [`AccountsOverlay::open`]).
#[derive(Default)]
struct Todo {
    /// Take the keyboard.
    focus: bool,
    /// Put the caret in the sign-in code box.
    focus_code: bool,
    /// Empty the sign-in code box.
    clear_code: bool,
    /// Empty the key form.
    clear_key: bool,
}

/// What a cap box lets through: up to two digits.
fn digits(text: &str) -> bool {
    text.len() <= 2 && text.bytes().all(|b| b.is_ascii_digit())
}

/// [`digits`] as the text box wants it.
fn two_digits(text: &str, _: &mut App) -> bool {
    digits(text)
}

/// The accounts card.
pub struct AccountsOverlay {
    source: Source,
    open: bool,
    focus: FocusHandle,
    /// What had the keyboard when the card opened, to give it back.
    restore: Option<FocusHandle>,
    todo: Todo,
    accounts: Load<AccountsDoc>,
    keys: Load<Vec<Key>>,
    caps: Load<HashMap<u32, Caps>>,
    meta: Meta,
    /// A button's request is out: no other is sent, and no read starts.
    busy: bool,
    notice: Option<Notice>,
    /// The key just made, `(name, secret)`: shown once.
    secret: Option<(String, String)>,
    /// The key whose `revoke` was clicked once.
    revoking: Option<String>,
    /// Made when the first frame has a window.
    fields: Option<Fields>,
    rows: HashMap<u32, CapRow>,
    reload: Option<Task<()>>,
    ticker: Option<Task<()>>,
    ticks: u32,
}

impl EventEmitter<Closed> for AccountsOverlay {}

impl Focusable for AccountsOverlay {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl AccountsOverlay {
    /// A closed card over the daemon `net` talks to (or over the fixture file
    /// `FEMME_ACCOUNTS_FIXTURE` names, which wins; with neither it says there is no daemon).
    pub fn new(net: Option<Entity<Net>>, cx: &mut Context<Self>) -> Self {
        Self {
            source: Source::pick(net),
            open: false,
            focus: cx.focus_handle(),
            restore: None,
            todo: Todo::default(),
            accounts: Load::Loading,
            keys: Load::Loading,
            caps: Load::Loading,
            meta: Meta::default(),
            busy: false,
            notice: None,
            secret: None,
            revoking: None,
            fields: None,
            rows: HashMap::new(),
            reload: None,
            ticker: None,
            ticks: 0,
        }
    }

    /// Shows the card and reads everything from the box.
    pub fn open(&mut self, cx: &mut Context<Self>) {
        if self.open {
            return;
        }
        self.open = true;
        self.todo.focus = true;
        self.restore = None;
        self.notice = None;
        self.revoking = None;
        self.secret = None;
        self.ticks = 0;
        self.refresh(cx);
        self.ticker = Some(cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(Duration::from_secs(1)).await;
            if !this.update(cx, |t, cx| t.tick(cx)).unwrap_or(false) {
                break;
            }
        }));
        cx.notify();
    }

    /// Hides the card; a cap that was still resting is sent first.
    pub fn close(&mut self, cx: &mut Context<Self>) {
        if !self.open {
            return;
        }
        self.flush_caps(cx);
        self.open = false;
        // The box shows a key's secret once.
        self.secret = None;
        cx.emit(Closed);
        cx.notify();
    }

    /// Whether the card is showing.
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Closes the card on the user's say-so and gives the keyboard back.
    fn dismiss(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let back = self.restore.take();
        self.close(cx);
        if let Some(handle) = back {
            window.focus(&handle, cx);
        }
    }

    /// The caps the box holds for account `number` (none set: both are the default).
    fn caps_of(&self, number: u32) -> Caps {
        match &self.caps {
            Load::Ready(caps) => caps.get(&number).copied().unwrap_or_default(),
            _ => Caps::default(),
        }
    }

    /// The numbers of the accounts the card lists.
    fn numbers(&self) -> Vec<u32> {
        match &self.accounts {
            Load::Ready(doc) => match &doc.list {
                AccountList::Rows(rows) => rows.iter().map(|a| a.number).collect(),
                _ => Vec::new(),
            },
            _ => Vec::new(),
        }
    }

    /// Everything a frame needs that takes the window: the text boxes (made on the first frame),
    /// the keyboard, and the cap boxes filled from what the daemon holds.
    fn sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.fields.is_none() {
            self.fields = Some(self.make_fields(window, cx));
        }
        // The card keeps the keyboard: when the box that had it goes (the sign-in card closes), it
        // comes back here, where Esc is heard.
        if self.todo.focus || !self.focus.contains_focused(window, cx) {
            self.todo.focus = false;
            cx.defer_in(window, |t, window, cx| {
                if t.restore.is_none() {
                    t.restore = window.focused(cx).filter(|h| !t.focus.contains(h, window));
                }
                window.focus(&t.focus, cx);
            });
        }
        if std::mem::take(&mut self.todo.focus_code) {
            cx.defer_in(window, |t, window, cx| {
                if let Some(f) = &t.fields {
                    f.code.update(cx, |s, cx| s.focus(window, cx));
                }
            });
        }
        if let Some(f) = &self.fields {
            if std::mem::take(&mut self.todo.clear_code) {
                f.code.update(cx, |s, cx| s.set_value("", window, cx));
            }
            if std::mem::take(&mut self.todo.clear_key) {
                f.key_name.update(cx, |s, cx| s.set_value("", window, cx));
                f.key_token.update(cx, |s, cx| s.set_value("", window, cx));
            }
        }
        let hint = self
            .meta
            .cap
            .map_or_else(|| "default".to_owned(), |c| c.to_string());
        for number in self.numbers() {
            if !self.rows.contains_key(&number) {
                let row = self.make_row(number, &hint, window, cx);
                self.rows.insert(number, row);
            }
            let held = self.caps_of(number);
            let Some(row) = self.rows.get_mut(&number) else {
                continue;
            };
            if row.hint != hint {
                row.five
                    .update(cx, |s, cx| s.set_placeholder(hint.clone(), window, cx));
                row.hint = hint.clone();
            }
            // What the daemon holds fills the boxes, unless the user is in the middle of an edit.
            if row.seen != Some(held) && row.debounce.is_none() && !row.saving {
                row.five.update(cx, |s, cx| {
                    s.set_value(cap_text(held.five_hour), window, cx)
                });
                row.week
                    .update(cx, |s, cx| s.set_value(cap_text(held.weekly), window, cx));
                row.seen = Some(held);
            }
        }
    }

    fn make_fields(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Fields {
        let code = cx.new(|cx| {
            InputState::new(window, cx).placeholder("2. Paste the code the sign-in page shows")
        });
        let key_name = cx.new(|cx| InputState::new(window, cx).placeholder("name, e.g. instinct"));
        let key_token = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("GitHub token (optional: or connect GitHub on the key after)")
                .masked(true)
        });
        let subs = vec![
            cx.subscribe_in(&code, window, |t, _, ev: &InputEvent, _, cx| {
                if matches!(ev, InputEvent::PressEnter { .. }) {
                    t.login_code(cx);
                }
            }),
            cx.subscribe_in(&key_name, window, |t, _, ev: &InputEvent, _, cx| {
                if matches!(ev, InputEvent::PressEnter { .. }) {
                    t.key_add(cx);
                }
            }),
            cx.subscribe_in(&key_token, window, |t, _, ev: &InputEvent, _, cx| {
                if matches!(ev, InputEvent::PressEnter { .. }) {
                    t.key_add(cx);
                }
            }),
        ];
        Fields {
            code,
            key_name,
            key_token,
            _subs: subs,
        }
    }

    /// Tells the card when one of account `number`'s cap boxes changes or gets Enter.
    fn watch(
        number: u32,
        state: &Entity<InputState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Subscription {
        cx.subscribe_in(
            state,
            window,
            move |t, _, ev: &InputEvent, _, cx| match ev {
                InputEvent::Change => t.cap_edited(number, cx),
                InputEvent::PressEnter { .. } => t.cap_commit(number, cx),
                _ => {}
            },
        )
    }

    fn make_row(
        &mut self,
        number: u32,
        hint: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> CapRow {
        let five = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(hint.to_owned())
                .validate(two_digits)
        });
        let week = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("default")
                .validate(two_digits)
        });
        let subs = [
            Self::watch(number, &five, window, cx),
            Self::watch(number, &week, window, cx),
        ];
        CapRow {
            five,
            week,
            seen: None,
            hint: hint.to_owned(),
            status: CapStatus::Idle,
            debounce: None,
            saving: false,
            _subs: subs,
        }
    }
}

impl Render for AccountsOverlay {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.open {
            return div().into_any_element();
        }
        self.sync(window, cx);
        self.layer(window.viewport_size(), cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_read_that_fails_keeps_what_is_shown() {
        let mut l: Load<u8> = Load::Loading;
        assert_eq!(l.settle(Err("no".into())), None);
        assert_eq!(l, Load::Failed("no".into()));
        assert_eq!(l.settle(Ok(3)), None);
        assert_eq!(l, Load::Ready(3));
        // Now there is something to lose: the reason is handed back, the data stays.
        assert_eq!(l.settle(Err("lost".into())), Some("lost".into()));
        assert_eq!(l, Load::Ready(3));
        assert_eq!(l.settle(Ok(4)), None);
        assert_eq!(l, Load::Ready(4));
    }

    #[test]
    fn a_cap_box_takes_two_digits_only() {
        for ok in ["", "5", "80", "99"] {
            assert!(digits(ok), "{ok}");
        }
        for bad in ["100", "8x", "-1", " 8", "٣"] {
            assert!(!digits(bad), "{bad}");
        }
    }
}
