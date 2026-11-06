//! The input box under the transcript: what the user types goes to the session as a prompt
//! (`Req::Prompt`), attached images and files are uploaded first (`Req::BlobPut`) and shown as
//! hoverable thumbnails. Owned by the composer agent (docs/desktop-ui.md §composer).
//!
//! The pure parts (what a blob is, the draft and its chip state machine, reading a file under the
//! upload cap) live in the submodules and are unit-tested; this file is the gpui view around them.
#![warn(missing_docs)]

mod chips;
mod draft;
mod files;
mod slash;
mod sniff;

use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use femme_proto::rpc::{PromptHow, Req, Resp};
use femme_proto::state::SessionStatus;
use femme_proto::Sid;
use gpui::{
    div, prelude::FluentBuilder as _, px, relative, App, AppContext as _, ClipboardEntry,
    ClipboardItem, Context, Div, Entity, ExternalPaths, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, KeyDownEvent, ParentElement as _, PathPromptOptions,
    Render, Rgba, SharedString, Stateful, StatefulInteractiveElement as _, Styled as _,
    Subscription, Task, Window,
};
use gpui_component::input::{Escape, InputEvent, Textarea, TextareaState};

use self::chips::Previews;
use self::draft::{
    join_notices, picker_notice, Chip, ChipId, Drafts, Outgoing, Target, UPLOAD_GONE,
};
use crate::net::{Link, Net};
use crate::theme::{
    self, BAD, BODY_SIZE, BTN_SIZE, FONT_MONO, FONT_PIXEL, META_SIZE, MUTED, NEON, PANEL, PAPER,
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
    /// The session changed: the next render puts its draft text into the input.
    reload: bool,
    /// The placeholder the input shows now.
    placeholder: &'static str,
    /// Whether the input holds no text, kept so the send button follows without re-reading it.
    input_empty: bool,
    /// Images pasted so far: names the next `paste-<n>.png`.
    pastes: u32,
    /// What the last render drew, to re-render only when the session's kind changes.
    last_target: Target,
    previews: Previews,
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
    _subs: Vec<Subscription>,
}

/// A prompt on its way: what to clear once the daemon has it.
struct Pending {
    sid: Sid,
    /// The text as typed (not trimmed), to tell whether the input still holds it.
    text: String,
    ids: Vec<ChipId>,
}

impl Composer {
    /// A composer with no session yet; `net` is the daemon link (`None`: nothing can be sent).
    pub fn new(net: Option<Entity<Net>>, cx: &mut Context<Self>) -> Self {
        let mut subs = Vec::new();
        if let Some(net) = &net {
            // The model changes on every event; only a change of the session's kind matters here.
            subs.push(cx.observe(net, |this, net, cx| {
                let up = matches!(net.read(cx).model.link, Link::Up(_));
                if up && !this.link_up {
                    this.asked = None;
                    this.ask_commands(cx);
                }
                this.link_up = up;
                if this.target(cx) != this.last_target {
                    cx.notify();
                }
            }));
        }
        Composer {
            net,
            session: None,
            focus: cx.focus_handle(),
            input: None,
            drafts: Drafts::default(),
            reload: true,
            placeholder: "",
            input_empty: true,
            pastes: 0,
            last_target: Target::NoSession,
            previews: Rc::default(),
            commands: Vec::new(),
            asked: None,
            link_up: false,
            pick: 0,
            query: String::new(),
            menu_off: false,
            menu_shown: false,
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
            self.drafts.park(old, text);
        }
        self.session = sid;
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
        target: Target,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.reload {
            self.reload = false;
            let text = self
                .session
                .map_or("", |sid| self.drafts.text(sid))
                .to_owned();
            self.input_empty = text.is_empty();
            input.update(cx, |input, cx| {
                input.set_value(text, window, cx);
                // Typing carries on after what was there, not before it.
                let end = input.text().len();
                input.set_selected_range(end..end, cx);
            });
        }
        let hint = target.placeholder();
        if hint != self.placeholder {
            self.placeholder = hint;
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
                let how = if *secondary {
                    PromptHow::Steer
                } else {
                    PromptHow::Queue
                };
                self.send(how, window, cx);
            }
            InputEvent::Change => {
                self.menu_off = false;
                let typing = input.read(cx).value().starts_with('/');
                if typing || self.menu_shown {
                    cx.notify();
                }
                let empty = input.read(cx).text().len() == 0;
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
            .into_iter()
            .map(str::to_owned)
            .collect()
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
        self.menu_off = false;
        if let Some(draft) = self.session.and_then(|sid| self.drafts.get_mut(sid)) {
            draft.notice = None;
        }
        cx.notify();
    }

    /// The `/` menu's keys, ahead of the input: Up/Down pick, Tab and Enter complete (Enter on a
    /// whole command name sends it as usual), Esc closes. With the menu closed every key goes on.
    fn on_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let rows = self.menu(cx);
        if rows.is_empty() {
            return;
        }
        let pick = self.pick.min(rows.len() - 1);
        let k = &ev.keystroke;
        match k.key.as_str() {
            "down" => self.pick = (pick + 1).min(rows.len() - 1),
            "up" => self.pick = pick.saturating_sub(1),
            "escape" => self.menu_off = true,
            "tab" => self.complete(&rows[pick], window, cx),
            "enter" if !k.modifiers.shift => {
                let text = self.input.as_ref().map(|i| i.read(cx).value());
                if text.is_some_and(|t| slash::exact(&t, &self.commands)) {
                    return;
                }
                self.complete(&rows[pick], window, cx);
            }
            _ => return,
        }
        cx.stop_propagation();
        cx.notify();
    }

    /// The `/` menu above the input: one ruled list of `/name` rows, the picked one lit.
    fn menu_list(&self, rows: Vec<String>, cx: &mut Context<Self>) -> Div {
        ruled(div().flex().flex_col().bg(PAPER)).children(rows.into_iter().enumerate().map(
            |(ix, name)| {
                row(SharedString::from(format!("c-slash-{ix}")), ix == self.pick)
                    .min_h(px(26.))
                    .font_family(FONT_MONO)
                    .text_size(BODY_SIZE)
                    .child(div().truncate().child(format!("/{name}")))
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

    fn interrupt(&mut self, cx: &mut Context<Self>) -> bool {
        let (Some(net), Some(sid)) = (self.net.clone(), self.session) else {
            return false;
        };
        let working = net
            .read(cx)
            .model
            .sessions
            .get(&sid)
            .is_some_and(|s| s.status == SessionStatus::Working);
        if !working {
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

    /// Sends the input's text and the ready chips as a prompt: `how` is `Queue` for Enter and
    /// `Steer` for Ctrl-Enter. Nothing is cleared until the daemon has answered.
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
        let draft = self.drafts.entry(sid);
        draft.sending = true;
        draft.notice = None;
        let answer = request(
            &net,
            Req::Prompt {
                sid,
                text: body,
                attachments,
                how,
            },
            cx,
        );
        let pending = Pending { sid, text, ids };
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
        let Pending { sid, text, ids } = pending;
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
                    }
                }
                self.drafts.prune(sid);
            }
        }
        cx.notify();
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

/// Lets go of what a chip that is gone held: its preview entity and gpui's decoded copy.
fn forget(previews: &Previews, chip: &Chip, cx: &mut App) {
    previews.borrow_mut().remove(&chip.id);
    if let Some(image) = &chip.image {
        image.clone().remove_asset(cx);
    }
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
        r: mix(PAPER.r, NEON.r),
        g: mix(PAPER.g, NEON.g),
        b: mix(PAPER.b, NEON.b),
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
            .bg(PANEL)
            .text_color(MUTED)
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
        self.sync_input(&input, target, window, cx);
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
            button_dark("c-send", "send").on_click(
                cx.listener(|this, _, window, cx| this.send(PromptHow::Queue, window, cx)),
            )
        } else {
            disabled_button("c-send", "send")
        };

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
                .bg(PAPER),
        );
        card.style().box_shadow = Some(theme::hard_shadow());
        let card = card
            .when(takes, |card| {
                card.drag_over::<ExternalPaths>(|style, _, _, _| {
                    style.border_color(NEON).bg(drop_fill())
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
                        .text_color(BAD)
                        .child(line),
                )
            });

        // No padding on the root: the shell's gutter is the room round the card, so that its border
        // lines up with the panes above it (the 3px hard shadow falls into that gutter).
        div()
            .id("composer")
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::on_escape))
            // Before the input: while the `/` menu shows, its keys are the menu's.
            .capture_key_down(cx.listener(Self::on_key))
            .w_full()
            .flex_none()
            .child(card)
    }
}
