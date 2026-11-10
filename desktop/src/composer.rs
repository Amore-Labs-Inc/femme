//! The input box under the transcript: what the user types goes to the session as a prompt
//! (`Req::Prompt`), attached images and files are uploaded first (`Req::BlobPut`) and shown as
//! hoverable thumbnails. Owned by the composer agent (docs/desktop-ui.md §composer).
//!
//! The pure parts (what a blob is, the draft and its chip state machine, reading a file under the
//! upload cap) live in the submodules and are unit-tested; this file is the gpui view around them.
#![warn(missing_docs)]

mod chips;
mod disk;
mod draft;
mod files;
mod history;
mod permission;
mod slash;
mod sniff;
mod xp;

use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use femme_client::Event;
use femme_proto::rpc::{PromptHow, Req, Resp, Scope};
use femme_proto::state::{Agent, Context as Xp, Row, SessionStatus};
use femme_proto::{OpId, Sid};
use gpui::{
    div, prelude::FluentBuilder as _, px, relative, App, AppContext as _, ClipboardEntry,
    ClipboardItem, Context, Div, Entity, ExternalPaths, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, Keystroke, KeystrokeEvent, ParentElement as _,
    PathPromptOptions, Render, Rgba, SharedString, Stateful, StatefulInteractiveElement as _,
    Styled as _, Subscription, Task, Window,
};
use gpui_component::input::{Escape, InputEvent, Textarea, TextareaState};
use gpui_component::tooltip::Tooltip;

use self::chips::Previews;
use self::draft::{
    join_notices, picker_notice, Chip, ChipId, Drafts, Outgoing, Target, UPLOAD_GONE,
};
use crate::net::{ClientEvent, Link, Net};
use crate::theme::{
    self, bad, muted, neon, panel, paper, BODY_SIZE, BTN_SIZE, FONT_MONO, FONT_PIXEL, META_SIZE,
};
use crate::ui::{button, button_dark, row, ruled};

/// The input is this many lines tall when empty...
const MIN_ROWS: usize = 2;
/// ...grows with the text up to this many, then scrolls.
const MAX_ROWS: usize = 8;
/// An empty or failed `Req::Commands` is asked again at most this often, so that a daemon deployed
/// later fills the `/` menu.
const COMMANDS_RETRY: Duration = Duration::from_secs(30);

/// The composer: the attachment chips, a multi-line input and the send button, for the session
/// the shell last gave it with [`Composer::set_session`]. What was typed and attached is kept per
/// session, so switching away and back loses nothing.
pub struct Composer {
    net: Option<Entity<Net>>,
    session: Option<Sid>,
    /// The element focus lands on before the input exists, and the one the shell may focus.
    focus: FocusHandle,
    /// Made on the first render: a `TextareaState` needs a `Window`, which `new` does not have.
    input: Option<Entity<TextareaState>>,
    drafts: Drafts,
    /// Where drafts are kept between runs (see [`disk`]); `None` when the environment names no home.
    disk: Option<std::path::PathBuf>,
    /// The session changed: the next render puts its draft text into the input.
    reload: bool,
    /// The placeholder the input shows now.
    placeholder: String,
    /// Whether the input holds no text, kept so the send button follows without re-reading it.
    input_empty: bool,
    /// Images pasted so far: names the next `paste-<n>.png`.
    pastes: u32,
    /// What the last render drew, to re-render only when the session's kind changes.
    last_target: Target,
    /// The permission mode the last render's pill showed, to re-render when it changes.
    last_permission: Option<String>,
    /// The context the XP bar last showed; the model is observed for a change of it.
    last_xp: Option<Xp>,
    /// When the bar last levelled up.
    level_up: xp::LevelUp,
    previews: Previews,
    /// The right-click menu of the chips, and the preview its `open` keeps on screen.
    ctx: Entity<crate::ui::ctx::Menu>,
    /// The daemon's slash commands (`Req::Commands`), names without the `/`; empty until it answers.
    commands: Vec<String>,
    /// When `Req::Commands` was last sent on this connection (`None`: not yet).
    asked: Option<Instant>,
    /// Whether the link was up at the last look, to ask again on each new connection.
    link_up: bool,
    /// The `/` menu's selected row.
    pick: usize,
    /// The query the menu was drawn for: another one selects the first row again.
    query: String,
    /// Esc closed the menu; the next edit opens it again.
    menu_off: bool,
    /// Whether the last render drew the menu, so that an edit that closes it re-renders.
    menu_shown: bool,
    /// What the user sent in each session, for Up and Down ([`history`]).
    history: HashMap<Sid, history::History>,
    /// Where Up and Down have got to in the selected session's history.
    browse: history::Browse,
    _subs: Vec<Subscription>,
}

/// A prompt on its way: what to clear once the daemon has it.
struct Pending {
    sid: Sid,
    /// The text as typed (not trimmed), to tell whether the input still holds it.
    text: String,
    ids: Vec<ChipId>,
    /// The op id it went under, which names its faded bubble in the transcript
    /// ([`crate::net::Model::outbox`]) until its stored message comes, and is dropped if refused.
    shown: OpId,
}

impl Composer {
    /// A composer with no session yet; `net` is the daemon link (`None`: nothing can be sent).
    pub fn new(net: Option<Entity<Net>>, cx: &mut Context<Self>) -> Self {
        let mut subs = Vec::new();
        let this = cx.weak_entity();
        subs.push(cx.intercept_keystrokes(move |ev, window, cx| {
            this.update(cx, |this, cx| this.intercept(ev, window, cx))
                .ok();
        }));
        if let Some(net) = &net {
            // The model changes on every event; only a change of the session's kind matters here.
            subs.push(cx.observe(net, |this, net, cx| {
                let up = matches!(net.read(cx).model.link, Link::Up(_));
                if up && !this.link_up {
                    this.asked = None;
                    this.ask_commands(cx);
                }
                this.link_up = up;
                if this.target(cx) != this.last_target
                    || this.permission(cx) != this.last_permission
                    || this.xp(cx) != this.last_xp
                    || this.hint(cx) != this.placeholder
                {
                    cx.notify();
                }
            }));
            // The session's own `UserMessage` rows, which the transcript's subscription brings in.
            subs.push(
                cx.subscribe(net, |this, _, ev: &ClientEvent, _| match &ev.0 {
                    Event::Changes {
                        scope: Scope::Session(sid),
                        rows,
                        ..
                    } => {
                        let h = this.history.entry(*sid).or_default();
                        for row in rows {
                            if let Row::Item(item) = row {
                                h.item(item);
                            }
                        }
                    }
                    Event::Reset {
                        scope: Scope::Session(sid),
                    } => {
                        if let Some(h) = this.history.get_mut(sid) {
                            h.reset();
                        }
                    }
                    _ => {}
                }),
            );
        }
        Composer {
            net,
            session: None,
            focus: cx.focus_handle(),
            input: None,
            drafts: Drafts::default(),
            disk: disk::default_dir(),
            reload: true,
            placeholder: String::new(),
            input_empty: true,
            pastes: 0,
            last_target: Target::NoSession,
            last_permission: None,
            last_xp: None,
            level_up: xp::LevelUp::default(),
            previews: Rc::default(),
            ctx: cx.new(crate::ui::ctx::Menu::new),
            commands: Vec::new(),
            asked: None,
            link_up: false,
            pick: 0,
            query: String::new(),
            menu_off: false,
            menu_shown: false,
            history: HashMap::new(),
            browse: history::Browse::default(),
            _subs: subs,
        }
    }

    /// Points the composer at `sid` (`None`: no session). The text typed for the session it
    /// leaves is parked, and the new session's is put back.
    pub fn set_session(&mut self, sid: Option<Sid>, cx: &mut Context<Self>) {
        if self.session == sid {
            return;
        }
        // While `reload` is pending the input still holds the text of a session already parked.
        if let (Some(old), Some(input), false) = (self.session, &self.input, self.reload) {
            let text = input.read(cx).value().to_string();
            self.keep(old, &text);
            self.drafts.park(old, text);
        }
        self.session = sid;
        self.browse.end();
        self.reload = true;
        self.menu_off = false;
        cx.notify();
    }

    /// Attaches `data` as `name` to the selected session's draft, as if it had been pasted or
    /// dropped: a chip with a thumbnail when the bytes are a picture, uploaded at once with
    /// `Req::BlobPut`, then through the same states (uploading, ready, failed with a retry) until
    /// the prompt goes out. For a caller elsewhere in the app that made a file, such as a drawing
    /// board handing over a PNG:
    /// `composer.update(cx, |c, cx| c.add_attachment("diagram-1.png".into(), png, cx))`.
    ///
    /// Nothing happens while no session that takes prompts is selected (none, or a shell). A file
    /// the composer refuses (empty, over `femme_proto::rpc::MAX_BLOB_PUT`, a seventeenth) leaves its
    /// reason under the input, as a drop does. The name is kept unless the draft already holds it
    /// (`diagram-1 (2).png`). Focus does not move: focus the composer afterwards so that the user
    /// can type the message.
    pub fn add_attachment(&mut self, name: String, data: Vec<u8>, cx: &mut Context<Self>) {
        let Some(sid) = self.session.filter(|_| self.target(cx).takes_prompts()) else {
            return;
        };
        let refusal = self.attach(sid, name, data, cx);
        self.settle_notice(sid, refusal.into_iter().collect(), cx);
    }

    /// What the selected session is, as far as typing into it goes.
    fn target(&self, cx: &App) -> Target {
        let agent = self
            .net
            .as_ref()
            .zip(self.session)
            .and_then(|(net, sid)| net.read(cx).model.sessions.get(&sid).map(|s| s.agent));
        Target::of(self.net.is_some(), agent)
    }

    /// The selected session's permission mode, when it is a Claude session whose mode is known.
    fn permission(&self, cx: &App) -> Option<String> {
        let (net, sid) = self.net.as_ref().zip(self.session)?;
        let session = net.read(cx).model.sessions.get(&sid)?;
        (session.agent == Agent::Claude)
            .then(|| session.permission.clone())
            .flatten()
    }

    /// The placeholder for the selected session: `message opus 5.5 · high` for a Claude one.
    fn hint(&self, cx: &App) -> String {
        let session = self
            .net
            .as_ref()
            .zip(self.session)
            .and_then(|(net, sid)| net.read(cx).model.sessions.get(&sid));
        self.target(cx).placeholder_with(
            session.and_then(|s| s.model.as_deref()),
            session.and_then(|s| s.effort.as_deref()),
        )
    }

    /// The selected session's context window, when it is a Claude session that reported one.
    fn xp(&self, cx: &App) -> Option<Xp> {
        let (net, sid) = self.net.as_ref().zip(self.session)?;
        let session = net.read(cx).model.sessions.get(&sid)?;
        (session.agent == Agent::Claude)
            .then_some(session.context)
            .flatten()
    }

    /// Shift+Tab or a click on the pill: `Req::CyclePermission`, which steps the session to the next
    /// mode as Shift+Tab does in Claude. The new mode comes back on the session's row; a refusal is
    /// shown under the input. Nothing is sent unless the selected session is a Claude one.
    fn cycle_permission(&mut self, cx: &mut Context<Self>) -> bool {
        let (Some(net), Some(sid)) = (self.net.clone(), self.session) else {
            return false;
        };
        if self.target(cx) != Target::Agent(Agent::Claude) {
            return false;
        }
        let answer = request(&net, Req::CyclePermission { sid }, cx);
        cx.spawn(async move |this, cx| {
            if let Err(why) = answer.await {
                this.update(cx, |this, cx| {
                    this.notice(sid, format!("permission mode: {why}"), cx)
                })
                .ok();
            }
        })
        .detach();
        true
    }

    /// Shift+Tab with the keyboard in the input steps a Claude session's permission mode, menu open
    /// or not. It is caught before key bindings resolve: the input binds it to outdent and the root
    /// to the previous focus stop, and both would run before a key-down listener.
    /// The `/` menu's keys go the same way, for the same reason (the input binds Tab, Up, Down,
    /// Enter and Esc).
    fn intercept(&mut self, ev: &KeystrokeEvent, window: &mut Window, cx: &mut Context<Self>) {
        let here = self
            .input
            .as_ref()
            .is_some_and(|i| i.read(cx).focus_handle(cx).contains_focused(window, cx));
        if !here {
            return;
        }
        let k = &ev.keystroke;
        let m = &k.modifiers;
        if k.key == "tab" && m.shift && !(m.control || m.alt || m.platform || m.function) {
            if self.cycle_permission(cx) {
                cx.stop_propagation();
            }
            return;
        }
        if !self.menu_key(k, window, cx) {
            self.history_key(k, window, cx);
        }
    }

    /// Up on the input's first row brings back the previous prompt sent in the session, again for
    /// older ones; Down on its last row goes forward, and past the newest gives back the draft
    /// typed before. Elsewhere in a multi-line text the keys move the caret as usual. Esc ends it.
    fn history_key(&mut self, k: &Keystroke, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(input), Some(sid)) = (self.input.clone(), self.session) else {
            return;
        };
        if k.key == "escape" {
            self.browse.end();
            return;
        }
        if k.modifiers.modified() || !matches!(k.key.as_str(), "up" | "down") {
            return;
        }
        if !self.target(cx).takes_prompts() {
            return;
        }
        let (first, last) = caret_edges(input.read(cx));
        let list = self.history.get(&sid).map(|h| h.list()).unwrap_or_default();
        let text = match k.key.as_str() {
            "up" if first => {
                let current = input.read(cx).value().to_string();
                self.browse.up(&list, &current)
            }
            "down" if last && self.browse.active() => self.browse.down(&list),
            _ => None,
        };
        let Some(text) = text else { return };
        input.update(cx, |input, cx| {
            input.set_value(text.clone(), window, cx);
            let end = input.text().len();
            input.set_selected_range(end..end, cx);
        });
        // `set_value` sends no `Change`; a recalled `/command` does not open the menu.
        self.input_empty = text.is_empty();
        self.menu_off = true;
        cx.stop_propagation();
        cx.notify();
    }

    fn ensure_input(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<TextareaState> {
        if let Some(input) = &self.input {
            return input.clone();
        }
        let input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(MIN_ROWS, MAX_ROWS)
                .submit_on_enter(true)
        });
        self._subs
            .push(cx.subscribe_in(&input, window, Self::on_input));
        // Focusing the composer (as the shell's focus cycle does) lands on its input.
        self._subs
            .push(cx.on_focus(&self.focus, window, |this, window, cx| {
                if let Some(input) = &this.input {
                    input.update(cx, |input, cx| input.focus(window, cx));
                }
            }));
        // The shell may have focused the placeholder handle before there was an input.
        if self.focus.is_focused(window) {
            input.update(cx, |input, cx| input.focus(window, cx));
        }
        self.input = Some(input.clone());
        input
    }

    /// Puts the session's draft text into the input after a switch, and the placeholder that says
    /// what the session is.
    fn sync_input(
        &mut self,
        input: &Entity<TextareaState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.reload {
            self.reload = false;
            let mut text = self
                .session
                .map_or("", |sid| self.drafts.text(sid))
                .to_owned();
            // A fresh run: what was typed for the session before the app last closed.
            if text.is_empty() {
                if let (Some(sid), Some(dir)) = (self.session, &self.disk) {
                    text = disk::load(dir, sid).unwrap_or_default();
                }
            }
            self.input_empty = text.is_empty();
            input.update(cx, |input, cx| {
                input.set_value(text, window, cx);
                // Typing carries on after what was there, not before it.
                let end = input.text().len();
                input.set_selected_range(end..end, cx);
            });
        }
        let hint = self.hint(cx);
        if hint != self.placeholder {
            self.placeholder = hint.clone();
            input.update(cx, |input, cx| input.set_placeholder(hint, window, cx));
        }
    }

    fn on_input(
        &mut self,
        input: &Entity<TextareaState>,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            // Shift-Enter has inserted its newline already.
            InputEvent::PressEnter {
                secondary,
                shift: false,
            } => {
                let text = input.read(cx).value().to_string();
                let how = slash::how_for(self.working(cx), *secondary, &text);
                self.send(how, window, cx);
            }
            InputEvent::Change => {
                self.menu_off = false;
                let typing = input.read(cx).value().starts_with('/');
                if typing || self.menu_shown {
                    cx.notify();
                }
                let empty = input.read(cx).text().len() == 0;
                if let Some(sid) = self.session {
                    self.keep(sid, &input.read(cx).value());
                }
                let stale = self
                    .session
                    .and_then(|sid| self.drafts.get_mut(sid))
                    .is_some_and(|d| d.notice.take().is_some());
                if stale || empty != self.input_empty {
                    self.input_empty = empty;
                    cx.notify();
                }
            }
            _ => {}
        }
    }

    /// `Req::Commands` for the `/` menu, once per connection; when the list came back empty or
    /// failed, again on a later call, at most every [`COMMANDS_RETRY`]. A daemon that does not know
    /// the request leaves the list empty, and the menu never shows.
    fn ask_commands(&mut self, cx: &mut Context<Self>) {
        let Some(net) = self.net.clone() else { return };
        if !matches!(net.read(cx).model.link, Link::Up(_)) {
            return;
        }
        let due = self
            .asked
            .is_none_or(|t| self.commands.is_empty() && t.elapsed() >= COMMANDS_RETRY);
        if !due {
            return;
        }
        self.asked = Some(Instant::now());
        let answer = request(&net, Req::Commands, cx);
        cx.spawn(async move |this, cx| {
            if let Ok(Resp::Commands { names }) = answer.await {
                this.update(cx, |this, cx| {
                    this.commands = names;
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }

    /// The `/` menu's rows for the input's text: none while it is closed, the session takes no
    /// prompts, or the text is not a command word being typed.
    fn menu(&self, cx: &App) -> Vec<String> {
        let Some(input) = self.input.as_ref().filter(|_| !self.menu_off) else {
            return Vec::new();
        };
        if !self.target(cx).takes_prompts() {
            return Vec::new();
        }
        let text = input.read(cx).value();
        slash::matches(&text, &self.commands)
    }

    /// Puts the picked command into the input, with a space for its arguments.
    fn complete(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(input) = self.input.clone() else {
            return;
        };
        input.update(cx, |input, cx| {
            input.set_value(slash::complete(name), window, cx);
            let end = input.text().len();
            input.set_selected_range(end..end, cx);
            input.focus(window, cx);
        });
        // `set_value` sends no `Change`: what an edit would do is done here.
        self.input_empty = false;
        if let Some(sid) = self.session {
            self.keep(sid, &slash::complete(name));
        }
        self.menu_off = false;
        if let Some(draft) = self.session.and_then(|sid| self.drafts.get_mut(sid)) {
            draft.notice = None;
        }
        // `/model`'s list: a Claude model asks for its effort next, then both are sent; DeepSeek
        // (no effort) switches at once
        let send = match slash::model_and_effort(name) {
            Some((model, effort)) => Some((model, Some(effort))),
            None => (name == "model deepseek").then(|| (format!("/{name}"), None)),
        };
        if let Some((model, effort)) = send {
            input.update(cx, |input, cx| input.set_value(model, window, cx));
            self.send(PromptHow::Queue, window, cx);
            // ponytail: sent alongside, not after the model's answer; fine while both are
            // instant CLI commands, a switch off DeepSeek restarts claude and may miss it
            if let (Some(effort), Some(net), Some(sid)) = (effort, self.net.clone(), self.session) {
                let req = Req::Prompt {
                    sid,
                    text: effort,
                    attachments: Vec::new(),
                    how: PromptHow::Queue,
                };
                request(&net, req, cx).detach();
            }
        }
        cx.notify();
    }

    /// The `/` menu's keys, ahead of the input's bindings: Up/Down pick, Tab and Enter complete
    /// (Enter on a whole command name sends it as usual), Esc closes. With the menu closed every
    /// key goes on. Whether the key was the menu's.
    fn menu_key(&mut self, k: &Keystroke, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let rows = self.menu(cx);
        if rows.is_empty() {
            return false;
        }
        let pick = self.pick.min(rows.len() - 1);
        match k.key.as_str() {
            "down" => self.pick = (pick + 1).min(rows.len() - 1),
            "up" => self.pick = pick.saturating_sub(1),
            "escape" => self.menu_off = true,
            "tab" if !k.modifiers.modified() => self.complete(&rows[pick], window, cx),
            "enter" if !k.modifiers.shift => {
                let text = self.input.as_ref().map(|i| i.read(cx).value());
                if text.is_some_and(|t| slash::exact(&t, &self.commands)) {
                    return true;
                }
                self.complete(&rows[pick], window, cx);
            }
            _ => return false,
        }
        cx.stop_propagation();
        cx.notify();
        true
    }

    /// The `/` menu above the input: one ruled list of `/name` rows, the picked one lit.
    fn menu_list(&self, rows: Vec<String>, cx: &mut Context<Self>) -> Div {
        ruled(div().flex().flex_col().bg(paper())).children(rows.into_iter().enumerate().map(
            |(ix, name)| {
                row(SharedString::from(format!("c-slash-{ix}")), ix == self.pick)
                    .min_h(px(26.))
                    .font_family(FONT_MONO)
                    .text_size(BODY_SIZE)
                    .child(div().truncate().child(format!("/{}", slash::label(&name))))
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.complete(&name, window, cx)),
                    )
            },
        ))
    }

    /// Esc interrupts a working session; otherwise it is left for whoever else binds it.
    fn on_escape(&mut self, _: &Escape, _: &mut Window, cx: &mut Context<Self>) {
        if !self.interrupt(cx) {
            cx.propagate();
        }
    }

    /// Whether the session on show is in a turn.
    fn working(&self, cx: &App) -> bool {
        self.net
            .as_ref()
            .zip(self.session)
            .and_then(|(net, sid)| net.read(cx).model.sessions.get(&sid))
            .is_some_and(|s| s.status == SessionStatus::Working)
    }

    fn interrupt(&mut self, cx: &mut Context<Self>) -> bool {
        let (Some(net), Some(sid)) = (self.net.clone(), self.session) else {
            return false;
        };
        if !self.working(cx) {
            return false;
        }
        let answer = request(&net, Req::Interrupt { sid }, cx);
        cx.spawn(async move |this, cx| {
            if let Err(why) = answer.await {
                this.update(cx, |this, cx| {
                    this.notice(sid, format!("interrupt failed: {why}"), cx)
                })
                .ok();
            }
        })
        .detach();
        true
    }

    /// Sends the input's text and the ready chips as a prompt, `how` per [`slash::how_for`]: in a
    /// working session Enter steers, ctrl-Enter interrupts first, a `/command` queues. Nothing is
    /// cleared until the daemon has answered.
    fn send(&mut self, how: PromptHow, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(net), Some(input), Some(sid)) =
            (self.net.clone(), self.input.clone(), self.session)
        else {
            return;
        };
        let text = input.read(cx).value().to_string();
        let target = self.target(cx);
        let gate = match self.drafts.get(sid) {
            Some(draft) => draft.gate(&text, target),
            None => draft::Draft::default().gate(&text, target),
        };
        let Outgoing {
            text: body,
            attachments,
            ids,
        } = match gate {
            Ok(out) => out,
            Err(block) => {
                if let Some(line) = block.notice() {
                    self.notice(sid, line, cx);
                }
                return;
            }
        };
        self.browse.end();
        self.history.entry(sid).or_default().sent(&text);
        let draft = self.drafts.entry(sid);
        draft.sending = true;
        draft.notice = None;
        // The transcript shows the prompt now, faded, until the daemon's stored copy arrives.
        let files = attachments.iter().map(|a| a.name.clone()).collect();
        let shown = OpId::new();
        net.update(cx, |net, cx| {
            cx.notify();
            net.model
                .outbox
                .push(sid, shown, body.clone(), files, Instant::now())
        });
        let answer = request_as(
            &net,
            shown,
            Req::Prompt {
                sid,
                text: body,
                attachments,
                how,
            },
            cx,
        );
        let pending = Pending {
            sid,
            text,
            ids,
            shown,
        };
        cx.spawn_in(window, async move |this, cx| {
            let result = answer.await;
            this.update_in(cx, |this, window, cx| {
                this.prompt_answered(pending, result, window, cx)
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn prompt_answered(
        &mut self,
        pending: Pending,
        result: Result<Resp, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Pending {
            sid,
            text,
            ids,
            shown,
        } = pending;
        // A `/` command is the agent's business: the daemon stores no message for it.
        if result.is_err() || text.trim_start().starts_with('/') {
            if let Some(net) = &self.net {
                net.update(cx, |net, cx| {
                    if net.model.outbox.forget(sid, shown) {
                        cx.notify();
                    }
                });
            }
        }
        let Some(draft) = self.drafts.get_mut(sid) else {
            return;
        };
        draft.sending = false;
        match result {
            // The text stays, and what went wrong is shown under it. An upload the daemon has
            // dropped since goes up again, so the next send has it.
            Err(why) => {
                let dropped = why.contains(UPLOAD_GONE);
                draft.notice = Some(why);
                let again: Vec<ChipId> = if dropped {
                    ids.into_iter().filter(|id| draft.expire(*id)).collect()
                } else {
                    Vec::new()
                };
                for id in again {
                    self.upload(sid, id, cx);
                }
            }
            Ok(_) => {
                draft.notice = None;
                let gone = draft.sent(&ids);
                if draft.text == text {
                    draft.text.clear();
                }
                for chip in &gone {
                    forget(&self.previews, chip, cx);
                }
                // Only what was sent goes: text typed while the prompt was on its way stays.
                if let (true, Some(input)) = (self.session == Some(sid), &self.input) {
                    if input.read(cx).value() == text {
                        input.update(cx, |input, cx| input.set_value("", window, cx));
                        self.input_empty = true;
                        self.keep(sid, "");
                    }
                }
                self.drafts.prune(sid);
            }
        }
        cx.notify();
    }

    /// Keeps `text` for `sid` on disk (see [`disk`]), so it is back after a quit. A draft that
    /// cannot be written is only lost on quit: not worth a notice under the input.
    fn keep(&self, sid: Sid, text: &str) {
        if let Some(dir) = &self.disk {
            if let Err(e) = disk::save(dir, sid, text) {
                eprintln!("femme-desktop: draft of {sid} not saved: {e}");
            }
        }
    }

    /// Shows `line` under the input of `sid`'s draft.
    fn notice(&mut self, sid: Sid, line: String, cx: &mut Context<Self>) {
        self.drafts.entry(sid).notice = Some(line);
        cx.notify();
    }

    /// A pasted clipboard: pictures and copied files become chips; plain text is left to the
    /// input (the answer says whether the paste was taken).
    fn paste(&mut self, item: &ClipboardItem, cx: &mut Context<Self>) -> bool {
        let Some(sid) = self.session.filter(|_| self.target(cx).takes_prompts()) else {
            return false;
        };
        let mut refusals = Vec::new();
        let mut taken = false;
        for entry in &item.entries {
            match entry {
                ClipboardEntry::Image(image) => {
                    self.pastes += 1;
                    let name = sniff::paste_name(self.pastes, image.format);
                    refusals.extend(self.attach(sid, name, image.bytes.clone(), cx));
                    taken = true;
                }
                ClipboardEntry::ExternalPaths(paths) => {
                    self.add_paths(sid, paths.paths().to_vec(), cx);
                    taken = true;
                }
                ClipboardEntry::String(_) => {}
            }
        }
        if taken {
            self.settle_notice(sid, refusals, cx);
        }
        taken
    }

    /// Reads `paths` off the UI thread and attaches each to `sid`'s draft.
    fn add_paths(&mut self, sid: Sid, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let loaded = cx
                .background_spawn(async move {
                    paths.iter().map(|p| files::load(p)).collect::<Vec<_>>()
                })
                .await;
            this.update(cx, |this, cx| {
                let mut refusals = Vec::new();
                for file in loaded {
                    match file {
                        Ok((name, data)) => refusals.extend(this.attach(sid, name, data, cx)),
                        Err(refusal) => refusals.push(refusal.to_string()),
                    }
                }
                this.settle_notice(sid, refusals, cx);
            })
            .ok();
        })
        .detach();
    }

    /// The `+` button: the platform's file picker.
    fn pick(&mut self, cx: &mut Context<Self>) {
        let Some(sid) = self.session else { return };
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: None,
        });
        cx.spawn(async move |this, cx| {
            let line = match picked.await {
                Ok(Ok(Some(paths))) => {
                    this.update(cx, |this, cx| this.add_paths(sid, paths, cx))
                        .ok();
                    return;
                }
                // Cancelled.
                Ok(Ok(None)) | Err(_) => return,
                // Linux without a file chooser portal says so here.
                Ok(Err(why)) => picker_notice(&why.to_string()),
            };
            this.update(cx, |this, cx| this.notice(sid, line, cx)).ok();
        })
        .detach();
    }

    /// Adds a chip to `sid`'s draft and starts its upload; the refusal's line when it is not taken.
    fn attach(
        &mut self,
        sid: Sid,
        name: String,
        data: Vec<u8>,
        cx: &mut Context<Self>,
    ) -> Option<String> {
        let id = self.drafts.new_id();
        match self.drafts.entry(sid).add(id, &name, data).map(|_| ()) {
            Ok(()) => {
                self.upload(sid, id, cx);
                None
            }
            Err(refusal) => Some(refusal.to_string()),
        }
    }

    /// Shows what a batch of attachments refused, in one line, under `sid`'s input; with nothing
    /// refused it takes the old line down, as any edit does.
    fn settle_notice(&mut self, sid: Sid, refusals: Vec<String>, cx: &mut Context<Self>) {
        self.drafts.entry(sid).notice = join_notices(refusals);
        cx.notify();
    }

    /// `Req::BlobPut` for the chip; the reply moves it to `Ready` or `Failed`.
    fn upload(&mut self, sid: Sid, id: ChipId, cx: &mut Context<Self>) {
        let Some(net) = self.net.clone() else { return };
        let Some(chip) = self.drafts.get(sid).and_then(|d| d.chip(id)) else {
            return;
        };
        let put = Req::BlobPut {
            name: chip.name.clone(),
            data: chip.data.to_vec(),
        };
        let answer = request(&net, put, cx);
        cx.spawn(async move |this, cx| {
            let result = answer.await;
            this.update(cx, |this, cx| {
                // A reply for a chip that is gone (removed, sent) finds nothing and changes nothing.
                if let Some(draft) = this.drafts.get_mut(sid) {
                    match result {
                        Ok(Resp::BlobHash(hash)) => draft.uploaded(id, hash),
                        Ok(_) => draft.failed(id, "the daemon answered something else".into()),
                        Err(why) => draft.failed(id, why),
                    };
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn retry_chip(&mut self, id: ChipId, cx: &mut Context<Self>) {
        let Some(sid) = self.session else { return };
        if self.drafts.get_mut(sid).is_some_and(|d| d.retry(id)) {
            self.upload(sid, id, cx);
            cx.notify();
        }
    }

    fn remove_chip(&mut self, id: ChipId, cx: &mut Context<Self>) {
        let Some(sid) = self.session else { return };
        if let Some(chip) = self.drafts.get_mut(sid).and_then(|d| d.remove(id)) {
            forget(&self.previews, &chip, cx);
            self.drafts.prune(sid);
            cx.notify();
        }
    }
}

/// Whether the input's caret is on its first and on its last row, wrapped rows counted once the
/// input has been laid out.
fn caret_edges(input: &TextareaState) -> (bool, bool) {
    let text = input.value();
    let cursor = input.cursor();
    let map = input.display_map();
    let rows = map.display_row_count();
    if rows == 0 {
        return history::edges(&text, cursor);
    }
    let (line, col) = history::line_col(&text, cursor);
    let row = map
        .buffer_pos_to_display_pos(gpui_component::input::BufferPoint::new(line, col))
        .row;
    (row == 0, row + 1 >= rows)
}

/// Lets go of what a chip that is gone held: its preview entity and gpui's decoded copy.
fn forget(previews: &Previews, chip: &Chip, cx: &mut App) {
    previews.borrow_mut().remove(&chip.id);
    if let Some(image) = &chip.image {
        image.clone().remove_asset(cx);
    }
}

/// [`request`] under the op id `op` (a prompt's, which its stored message keeps).
fn request_as(
    net: &Entity<Net>,
    op: OpId,
    req: Req,
    cx: &mut Context<Composer>,
) -> Task<Result<Resp, String>> {
    let (client, rt) = {
        let net = net.read(cx);
        (net.client(), net.rt())
    };
    let sent = rt.spawn(async move { client.request_as(op, req).await });
    cx.spawn(async move |_, _| match sent.await {
        Ok(Ok(resp)) => Ok(resp),
        Ok(Err(why)) => Err(why.to_string()),
        Err(why) => Err(why.to_string()),
    })
}

/// Runs `req` on the daemon link. Unlike `Net::run` the error is this request's own: `Net::error`
/// is one slot, which the next request that succeeds clears.
fn request(net: &Entity<Net>, req: Req, cx: &mut Context<Composer>) -> Task<Result<Resp, String>> {
    let (client, rt) = {
        let net = net.read(cx);
        (net.client(), net.rt())
    };
    let sent = rt.spawn(async move { client.request(req).await });
    cx.spawn(async move |_, _| match sent.await {
        Ok(Ok(resp)) => Ok(resp),
        Ok(Err(why)) => Err(why.to_string()),
        Err(why) => Err(why.to_string()),
    })
}

/// The card's colour while files are dragged over it: paper with a fifth of neon pink in it.
/// Opaque on purpose, a see-through fill lets the desk's dots show and goes muddy.
fn drop_fill() -> Rgba {
    let mix = |paper: f32, neon: f32| paper * 0.8 + neon * 0.2;
    Rgba {
        r: mix(paper().r, neon().r),
        g: mix(paper().g, neon().g),
        b: mix(paper().b, neon().b),
        a: 1.,
    }
}

/// A button that cannot be pressed now: [`button`]'s box in panel grey with a muted label, and no
/// hover (gpui takes one hover style per element, and `button` has set its own).
fn disabled_button(id: &'static str, label: &'static str) -> Stateful<Div> {
    ruled(
        div()
            .id(id)
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .h(px(34.))
            .px(px(10.))
            .font_family(FONT_PIXEL)
            .text_size(BTN_SIZE)
            .line_height(relative(1.))
            .bg(panel())
            .text_color(muted())
            .cursor_default(),
    )
    .child(label)
}

impl Focusable for Composer {
    /// The input once it exists, else the placeholder handle (focusing that moves to the input).
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        match &self.input {
            Some(input) => input.read(cx).focus_handle(cx),
            None => self.focus.clone(),
        }
    }
}

impl Render for Composer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let input = self.ensure_input(window, cx);
        let target = self.target(cx);
        self.sync_input(&input, window, cx);
        self.last_target = target;

        let takes = target.takes_prompts();
        let text = input.read(cx).value();
        let query = slash::query(&text);
        if takes && query.is_some() {
            // ponytail: retried from render rather than a timer: it only matters once `/` is typed.
            self.ask_commands(cx);
        }
        let query = query.unwrap_or_default();
        if query != self.query {
            self.query = query.to_owned();
            self.pick = 0;
        }
        let menu = self.menu(cx);
        self.pick = self.pick.min(menu.len().saturating_sub(1));
        self.menu_shown = !menu.is_empty();
        let menu = (!menu.is_empty()).then(|| self.menu_list(menu, cx));

        let draft = self.session.and_then(|sid| self.drafts.get(sid));
        let chips = draft.map_or(&[][..], |d| &d.chips[..]);
        let notice = draft.and_then(|d| d.notice.clone());
        let sending = draft.is_some_and(|d| d.sending);
        let can_send = takes && !sending && (!self.input_empty || !chips.is_empty());

        let attach = if takes {
            button("c-attach", "+").on_click(cx.listener(|this, _, _, cx| this.pick(cx)))
        } else {
            disabled_button("c-attach", "+")
        };
        let send = if can_send {
            // the button is a plain Enter: it steers a working session, like the terminal's Enter
            button_dark("c-send", "send").on_click(cx.listener(|this, _, window, cx| {
                let text = this
                    .input
                    .as_ref()
                    .map(|i| i.read(cx).value().to_string())
                    .unwrap_or_default();
                let how = slash::how_for(this.working(cx), false, &text);
                this.send(how, window, cx);
            }))
        } else {
            disabled_button("c-send", "send")
        };

        let permission = self.permission(cx);
        self.last_permission = permission.clone();
        // A 10 px colour square above `+`: the mode's name is its tooltip, a click steps the mode.
        let square = permission.map(|mode| {
            let look = permission::look(&mode);
            let tip = look.tip.clone();
            // One id per mode: gpui keeps a tooltip by id, and the old mode's must not outlive
            // the click that changed it.
            ruled(div().size(px(10.)).bg(look.color))
                .id(SharedString::from(format!("c-permission-{mode}")))
                .cursor_pointer()
                .tooltip(move |window, cx| {
                    Tooltip::new(format!("{tip} (shift-tab)")).build(window, cx)
                })
                .on_click(cx.listener(|this, _, _, cx| {
                    this.cycle_permission(cx);
                }))
        });
        let attach = div()
            .flex()
            .flex_col()
            .items_center()
            .gap(px(5.))
            .children(square)
            .child(attach);
        // The context XP bar above `send`, the square's twin on the right.
        let ctx = self.xp(cx);
        self.last_xp = ctx;
        let ring = self
            .level_up
            .step(self.session, ctx.map(|c| xp::level(&c)), Instant::now());
        let send = div()
            .flex()
            .flex_col()
            .items_stretch()
            .gap(px(5.))
            .children(ctx.map(|c| xp::bar(c, ring)))
            .child(send);

        let this = cx.entity().downgrade();
        let field = Textarea::new(&input)
            .appearance(false)
            .bordered(false)
            .disabled(!takes)
            .text_size(BODY_SIZE)
            .on_paste(move |item, _, cx| {
                this.update(cx, |this, cx| this.paste(item, cx))
                    .unwrap_or(false)
            });

        let mut card = ruled(
            div()
                .id("composer-card")
                .flex()
                .flex_col()
                .gap(px(8.))
                .p(px(8.))
                .bg(paper()),
        );
        card.style().box_shadow = Some(theme::hard_shadow());
        let card = card
            .when(takes, |card| {
                card.drag_over::<ExternalPaths>(|style, _, _, _| {
                    style.border_color(neon()).bg(drop_fill())
                })
                .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| {
                    if let Some(sid) = this.session {
                        this.add_paths(sid, paths.paths().to_vec(), cx);
                    }
                }))
            })
            .when(!chips.is_empty(), |card| {
                card.child(chips::row(chips, &self.previews, cx))
            })
            .when_some(menu, |card, menu| card.child(menu))
            .child(
                div()
                    .flex()
                    .items_end()
                    .gap(px(8.))
                    .child(attach)
                    .child(div().flex_1().min_w_0().child(field))
                    .child(send),
            )
            .when_some(notice, |card, line| {
                card.child(
                    div()
                        .font_family(FONT_MONO)
                        .text_size(META_SIZE)
                        .text_color(bad())
                        .child(line),
                )
            });

        // No padding on the root: the shell's gutter is the room round the card, so that its border
        // lines up with the panes above it (the 3px hard shadow falls into that gutter).
        div()
            .id("composer")
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::on_escape))
            .w_full()
            .flex_none()
            .child(card)
            .child(self.ctx.clone())
    }
}
