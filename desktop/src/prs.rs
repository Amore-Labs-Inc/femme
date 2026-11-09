//! The pull requests overlay (femme ▸ Pull requests): the box repo's open pull requests as a card
//! over the window. The list on the left (number, title, branch, +adds/−dels, author), the selected
//! one on the right: its title, its link, its description as Markdown, "Watch demo" when the branch
//! has a demo video, and "Merge & deploy" / "Close PR", each asked twice (merging ships to prod).
//! "Merge & deploy" is always the main button, also on a pull request that conflicts with main
//! (`Pr::mergeable` is `Some(false)`): the daemon then queues the merge behind a Claude chat that
//! resolves the conflicts (`Pr::merging`), and merges and deploys by itself once it is green. While
//! that runs the row says "merging…" and the detail has no merge or fix button. A conflicting one
//! also has "Fix with Claude" on the side (`Req::PrFix` starts a FEMME Claude chat, `fix #n`, and
//! the card closes onto it); while a fix runs (`Pr::fixing`) the row says "fixing…" and the detail
//! has "Open fix chat". The card reads the list again every [`WATCH`] while anything is merging,
//! fixing or not worked out yet: at most [`WATCH_ROUNDS`] times per opening, except that a merge it
//! queued is watched until it lands, card open or not, and its deploy is then watched as any merge.
//! The footer's "Auto-fix conflicts after a merge" is `Req::PrAutoFix`.
//!
//! The data is the daemon's: `Req::Prs` lists them, `Req::PrVideo` fetches a demo, `Req::PrMerge`
//! and `Req::PrClose` act. After a merge the card asks `Req::ServerInfo` every [`POLL`] until the
//! commit the box runs changes (the box deploys main by itself), for at most [`POLL_FOR`], and says
//! "deploying…" then "deployed <sha>". A daemon that does not know the requests (`Unsupported`)
//! makes the card say it is too old instead of failing.
//!
//! # Mounting
//!
//! As the accounts card: the shell renders the entity in a layer as big as the centre while
//! [`PrsOverlay::is_open`], and calls [`PrsOverlay::open`] from its menu. The card takes the
//! keyboard when it opens (once: the lightbox a demo opens in takes it from there and gives it back)
//! and gives it back to whatever had it when it closes by itself (Esc, `‹`, a click outside), and
//! emits [`Closed`] then.

mod view;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use femme_proto::rpc::{Pr, Req, Resp};
use femme_proto::Sid;
use gpui::{
    div, App, Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement, Render, Task,
    Window,
};

use crate::lightbox::Lightbox;
use crate::net::Net;
use crate::transcript::markdown::{self, Doc};
use crate::video::{self, Clip};

/// How often the card asks the box which commit it runs while a merge deploys.
pub const POLL: Duration = Duration::from_secs(5);

/// How long it keeps asking: the deploy timer plus a build, with room.
pub const POLL_FOR: Duration = Duration::from_secs(600);

/// How often the card reads the list again while a fix runs or GitHub has not said whether a pull
/// request merges cleanly.
pub const WATCH: Duration = Duration::from_secs(10);

/// The most such reads per opening of the card (ten minutes' worth).
pub const WATCH_ROUNDS: u32 = 60;

/// Emitted when the card closes: by itself (Esc, `‹`, a click outside) or through
/// [`PrsOverlay::close`].
pub struct Closed;

/// The list while it comes in.
#[derive(Clone, Debug, PartialEq)]
enum Load {
    Loading,
    /// The read failed and there is nothing older to show: why.
    Failed(String),
    Ready(Vec<Pr>),
}

/// The line under the title bar: the last refusal (red) or confirmation (green).
struct Notice {
    text: String,
    bad: bool,
}

/// An action clicked once, waiting for its second click.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Confirm {
    Merge(u64),
    Close(u64),
}

/// Where the last merge stands on its way to the box.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Deploy {
    /// Merged; the box still runs the old commit.
    Waiting {
        /// The pull request merged.
        number: u64,
        /// The commit the box ran before the merge (`None`: it did not say).
        from: Option<String>,
        /// The answers so far, one every [`POLL`].
        polls: u32,
    },
    /// The box runs the merge now.
    Done {
        /// The pull request merged.
        number: u64,
        /// The commit the box runs.
        sha: String,
    },
    /// [`POLL_FOR`] went by with the box on the old commit.
    GaveUp {
        /// The pull request merged.
        number: u64,
    },
}

/// The detail's buttons for a pull request, besides "Watch demo" and "Close PR".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Actions {
    /// "Merge & deploy", the main button: always, unless a queued merge is already running.
    pub merge: bool,
    /// The side button that steers a conflict fix.
    pub fix: Option<FixButton>,
}

/// The side button for a conflict fix.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FixButton {
    /// It conflicts with main and nothing is fixing it: "Fix with Claude".
    Start,
    /// This chat is fixing it: "Open fix chat".
    Open(Sid),
}

/// Which buttons `pr` gets. While a queued merge runs (`Pr::merging`) there is nothing to merge or
/// start, but its fix chat is still a click away.
pub fn actions(pr: &Pr) -> Actions {
    let fix = match (pr.fixing, pr.mergeable) {
        (Some(sid), _) => Some(FixButton::Open(sid)),
        (None, Some(false)) if !pr.merging => Some(FixButton::Start),
        _ => None,
    };
    Actions {
        merge: !pr.merging,
        fix,
    }
}

/// Whether the detail says, under the buttons, what a merge of `pr` does: it conflicts, so Claude
/// resolves first.
pub fn merge_waits(pr: &Pr) -> bool {
    pr.mergeable == Some(false) && !pr.merging
}

/// The mark a list row carries next to its title.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Badge {
    /// A merge is queued behind a conflict fix (accent).
    Merging,
    /// A fix chat is working on it.
    Fixing,
    /// It conflicts with main.
    Conflicts,
    /// GitHub has not worked out yet whether it merges cleanly (faint).
    Checking,
}

/// The row's mark for `pr`; none when it merges cleanly.
pub fn badge(pr: &Pr) -> Option<Badge> {
    if pr.merging {
        return Some(Badge::Merging);
    }
    match (pr.fixing, pr.mergeable) {
        (Some(_), _) => Some(Badge::Fixing),
        (None, Some(false)) => Some(Badge::Conflicts),
        (None, None) => Some(Badge::Checking),
        (None, Some(true)) => None,
    }
}

/// Whether the list is worth reading again soon: a queued merge or a fix is running, or GitHub has
/// not said whether one merges cleanly.
pub fn unsettled(prs: &[Pr]) -> bool {
    prs.iter()
        .any(|p| p.merging || p.fixing.is_some() || p.mergeable.is_none())
}

/// What a `PrMerge` that answered `Ok` did, read off the list that came after it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MergeOk {
    /// Merged: the deploy is next.
    Merged,
    /// Queued behind a conflict fix: it merges by itself.
    Queued,
}

/// `number`'s fate after an `Ok` merge: still listed and `merging`, queued; otherwise merged (a
/// list that still names it without `merging` is GitHub catching up).
pub fn merge_ok(prs: &[Pr], number: u64) -> MergeOk {
    match prs.iter().find(|p| p.number == number) {
        Some(p) if p.merging => MergeOk::Queued,
        _ => MergeOk::Merged,
    }
}

/// A queued merge's fate in a new list: gone from it, it merged (`Some(true)`); still `merging`,
/// running (`None`); listed but no longer merging, it stopped without merging (`Some(false)`).
pub fn queued_outcome(prs: &[Pr], number: u64) -> Option<bool> {
    match prs.iter().find(|p| p.number == number) {
        None => Some(true),
        Some(p) if p.merging => None,
        Some(_) => Some(false),
    }
}

/// Whether a failed merge failed because the branch conflicts with main.
pub fn is_conflict(why: &str) -> bool {
    why.contains("merge conflicts")
}

/// A failed merge as the card says it: a conflict plainly, with what to do; anything else as is.
pub fn merge_failure(number: u64, why: &str) -> String {
    if is_conflict(why) {
        format!("#{number} conflicts with main — Fix with Claude")
    } else {
        format!("#{number} not merged: {}", explain(why))
    }
}

impl Deploy {
    /// The line the card shows for it, and whether it is good news.
    pub fn line(&self) -> (String, bool) {
        match self {
            Deploy::Waiting { number, polls, .. } => {
                let secs = u64::from(*polls) * POLL.as_secs();
                let time = if secs < 60 {
                    format!("{secs}s")
                } else {
                    format!("{}m {:02}s", secs / 60, secs % 60)
                };
                (format!("#{number} merged · deploying… {time}"), true)
            }
            Deploy::Done { number, sha } => (format!("#{number} deployed {}", short(sha)), true),
            Deploy::GaveUp { number } => (
                format!(
                    "#{number} merged, but the box still runs the old commit after {} min",
                    POLL_FOR.as_secs() / 60
                ),
                false,
            ),
        }
    }
}

/// Whether the box has moved off `from`: it says which commit it runs, and that is another one.
pub fn landed(from: Option<&str>, now: Option<&str>) -> bool {
    now.is_some() && now != from
}

/// The first seven characters of a commit.
pub fn short(sha: &str) -> &str {
    sha.char_indices().nth(7).map_or(sha, |(i, _)| &sha[..i])
}

/// A request's failure as the card says it: a daemon that does not know it is too old, plainly.
pub fn explain(why: &str) -> String {
    if why.contains("Unsupported") {
        format!("daemon too old for pull requests ({why})")
    } else {
        why.to_owned()
    }
}

/// The pull requests card.
pub struct PrsOverlay {
    net: Option<Entity<Net>>,
    open: bool,
    focus: FocusHandle,
    /// What had the keyboard when the card opened, to give it back.
    restore: Option<FocusHandle>,
    /// Take the keyboard on the next frame.
    want_focus: bool,
    list: Load,
    /// The pull request shown on the right.
    selected: Option<u64>,
    /// Descriptions parsed so far, by number; emptied by every read of the list.
    docs: HashMap<u64, Doc>,
    /// What a button's request is doing ("merging"): no other is sent meanwhile.
    busy: Option<&'static str>,
    confirm: Option<Confirm>,
    notice: Option<Notice>,
    deploy: Option<Deploy>,
    reload: Option<Task<()>>,
    action: Option<Task<()>>,
    poll: Option<Task<()>>,
    /// Reads the list every [`WATCH`] while it is [`unsettled`].
    watch: Option<Task<()>>,
    /// Reads [`watch`](Self::watch) made since the card opened.
    watched: u32,
    /// A merge the daemon said `Ok` to, until the next list tells whether it merged or queued:
    /// its number and the commit the box ran before it.
    pending: Option<(u64, Option<String>)>,
    /// Merges queued behind a conflict fix: watched until they leave the list (merged).
    queued: Vec<u64>,
    /// Whether merges start fixes (`Req::PrAutoFix`); `None` until the daemon says (or when it
    /// does not know the request), and then the footer has no toggle.
    auto_fix: Option<bool>,
    /// The `PrAutoFix` read or change in flight.
    auto: Option<Task<()>>,
}

impl EventEmitter<Closed> for PrsOverlay {}

impl Focusable for PrsOverlay {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl PrsOverlay {
    /// A closed card over the daemon `net` talks to (with none it says there is no daemon).
    pub fn new(net: Option<Entity<Net>>, cx: &mut Context<Self>) -> Self {
        Self {
            net,
            open: false,
            focus: cx.focus_handle(),
            restore: None,
            want_focus: false,
            list: Load::Loading,
            selected: None,
            docs: HashMap::new(),
            busy: None,
            confirm: None,
            notice: None,
            deploy: None,
            reload: None,
            action: None,
            poll: None,
            watch: None,
            watched: 0,
            pending: None,
            queued: Vec::new(),
            auto_fix: None,
            auto: None,
        }
    }

    /// Shows the card and reads the list.
    pub fn open(&mut self, cx: &mut Context<Self>) {
        if self.open {
            return;
        }
        self.open = true;
        self.want_focus = true;
        self.restore = None;
        self.confirm = None;
        // A failure from last time is old news; a deploy in flight is not.
        self.notice = None;
        self.watched = 0;
        self.refresh(cx);
        self.read_auto_fix(cx);
        cx.notify();
    }

    /// Hides the card. A deploy being watched goes on being watched.
    pub fn close(&mut self, cx: &mut Context<Self>) {
        if !self.open {
            return;
        }
        self.open = false;
        self.confirm = None;
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

    fn send(&self, req: Req, cx: &mut Context<Self>) -> Task<Result<Resp, String>> {
        match &self.net {
            Some(net) => net.update(cx, |n, cx| n.try_run(req, cx)),
            None => Task::ready(Err(
                "no daemon: the desktop is running on synthetic panes".into()
            )),
        }
    }

    fn prs(&self) -> &[Pr] {
        match &self.list {
            Load::Ready(prs) => prs,
            _ => &[],
        }
    }

    /// The pull request on the right.
    fn current(&self) -> Option<&Pr> {
        let n = self.selected?;
        self.prs().iter().find(|p| p.number == n)
    }

    /// Reads the list again; a failure keeps what is shown and says why.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        let sent = self.send(Req::Prs, cx);
        self.reload = Some(cx.spawn(async move |this, cx| {
            let answer = sent.await;
            this.update(cx, |t, cx| {
                t.reload = None;
                match answer {
                    Ok(Resp::Prs(prs)) => {
                        t.take(prs);
                        t.settle_merges(cx);
                        t.watch_list(cx);
                    }
                    Ok(other) => {
                        t.fail(format!("unexpected answer: {other:?}"));
                        t.unknown_merge(cx);
                    }
                    Err(why) => {
                        t.fail(explain(&why));
                        t.unknown_merge(cx);
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn take(&mut self, prs: Vec<Pr>) {
        self.docs.clear();
        if !prs.iter().any(|p| Some(p.number) == self.selected) {
            self.selected = prs.first().map(|p| p.number);
        }
        self.list = Load::Ready(prs);
    }

    /// Reads what a fresh list says about the merge just answered and the merges queued: merged
    /// ones start the deploy watch, a queued one joins [`queued`](Self::queued).
    fn settle_merges(&mut self, cx: &mut Context<Self>) {
        if let Some((number, from)) = self.pending.take() {
            match merge_ok(self.prs(), number) {
                MergeOk::Merged => self.deploying(number, from, cx),
                MergeOk::Queued => {
                    if !self.queued.contains(&number) {
                        self.queued.push(number);
                    }
                    self.say(
                        format!(
                            "#{number} conflicts with main: Claude resolves them, then it merges \
                             and deploys by itself"
                        ),
                        false,
                    );
                }
            }
        }
        let outcomes: Vec<(u64, Option<bool>)> = self
            .queued
            .iter()
            .map(|&n| (n, queued_outcome(self.prs(), n)))
            .collect();
        for (number, outcome) in outcomes {
            let Some(merged) = outcome else {
                continue;
            };
            self.queued.retain(|&n| n != number);
            if merged {
                self.deploying_now(number, cx);
            } else {
                self.say(
                    format!("#{number} not merged: the conflict fix stopped"),
                    true,
                );
            }
        }
    }

    /// The list after a merge's `Ok` did not come: the merge most likely went through (the daemon
    /// restarts as a deploy lands), so watch the deploy.
    fn unknown_merge(&mut self, cx: &mut Context<Self>) {
        if let Some((number, from)) = self.pending.take() {
            self.deploying(number, from, cx);
        }
    }

    /// `number` merged while the box ran `from`: watch the box for the new commit.
    fn deploying(&mut self, number: u64, from: Option<String>, cx: &mut Context<Self>) {
        self.deploy = Some(Deploy::Waiting {
            number,
            from,
            polls: 0,
        });
        self.watch_deploy(cx);
        cx.notify();
    }

    /// A queued merge just left the list: note the commit the box runs now and watch for the next.
    fn deploying_now(&mut self, number: u64, cx: &mut Context<Self>) {
        let info = self.send(Req::ServerInfo, cx);
        cx.spawn(async move |this, cx| {
            let from = match info.await {
                Ok(Resp::ServerInfo { deployed, .. }) => deployed,
                _ => None,
            };
            this.update(cx, |t, cx| t.deploying(number, from, cx)).ok();
        })
        .detach();
    }

    /// Whether the list is worth reading again: a merge this card queued is running (card open or
    /// not), or the card is open, the list [`unsettled`] and [`WATCH_ROUNDS`] not used up.
    fn keep_watching(&self) -> bool {
        !self.queued.is_empty()
            || (self.open && self.watched < WATCH_ROUNDS && unsettled(self.prs()))
    }

    /// Reads the list every [`WATCH`] while [`keep_watching`](Self::keep_watching).
    fn watch_list(&mut self, cx: &mut Context<Self>) {
        if self.watch.is_some() || !self.keep_watching() {
            return;
        }
        self.watch = Some(cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(WATCH).await;
            let go = this
                .update(cx, |t, cx| {
                    if !t.keep_watching() {
                        t.watch = None;
                        return false;
                    }
                    t.watched += 1;
                    if t.reload.is_none() {
                        t.refresh(cx);
                    }
                    true
                })
                .unwrap_or(false);
            if !go {
                return;
            }
        }));
    }

    /// Asks the daemon whether merges start fixes; a daemon that does not know leaves no toggle.
    fn read_auto_fix(&mut self, cx: &mut Context<Self>) {
        let sent = self.send(Req::PrAutoFix { on: None }, cx);
        self.auto = Some(cx.spawn(async move |this, cx| {
            let answer = sent.await;
            this.update(cx, |t, cx| {
                t.auto = None;
                t.auto_fix = match answer {
                    Ok(Resp::PrAutoFix(on)) => Some(on),
                    _ => None,
                };
                cx.notify();
            })
            .ok();
        }));
    }

    /// The footer's toggle: flips whether merges start fixes.
    fn toggle_auto_fix(&mut self, cx: &mut Context<Self>) {
        let Some(cur) = self.auto_fix else {
            return;
        };
        if self.auto.is_some() {
            return;
        }
        let sent = self.send(Req::PrAutoFix { on: Some(!cur) }, cx);
        self.auto = Some(cx.spawn(async move |this, cx| {
            let answer = sent.await;
            this.update(cx, |t, cx| {
                t.auto = None;
                match answer {
                    Ok(Resp::PrAutoFix(on)) => t.auto_fix = Some(on),
                    Ok(other) => t.say(format!("unexpected answer: {other:?}"), true),
                    Err(why) => t.say(format!("can't change auto-fix: {}", explain(&why)), true),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// Starts a Claude chat that resolves `number`'s conflicts with main, and shows it.
    fn fix(&mut self, number: u64, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        self.busy = Some("starting the fix");
        self.confirm = None;
        self.notice = None;
        let sent = self.send(Req::PrFix { number }, cx);
        self.action = Some(cx.spawn(async move |this, cx| {
            let answer = sent.await;
            this.update(cx, |t, cx| {
                t.busy = None;
                match answer {
                    Ok(Resp::Session(sid)) => t.show_chat(sid, cx),
                    Ok(other) => t.say(format!("unexpected answer: {other:?}"), true),
                    Err(why) => t.say(
                        format!("#{number}: no fix started: {}", explain(&why)),
                        true,
                    ),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// Puts the session `sid` in the centre (as a click on it in the sidebar does) and closes the
    /// card, so the user watches the fix and can steer it.
    fn show_chat(&mut self, sid: Sid, cx: &mut Context<Self>) {
        if let Some(net) = &self.net {
            net.update(cx, |n, cx| n.set_current(Some(sid), cx));
        }
        self.close(cx);
    }

    fn fail(&mut self, why: String) {
        if matches!(self.list, Load::Ready(_)) {
            self.notice = Some(Notice {
                text: why,
                bad: true,
            });
        } else {
            self.list = Load::Failed(why);
        }
    }

    fn select(&mut self, number: u64, cx: &mut Context<Self>) {
        if self.selected != Some(number) {
            self.selected = Some(number);
            self.confirm = None;
            cx.notify();
        }
    }

    /// Up and down move the selection through the list.
    fn step(&mut self, down: bool, cx: &mut Context<Self>) {
        let prs = self.prs();
        if prs.is_empty() {
            return;
        }
        let at = prs.iter().position(|p| Some(p.number) == self.selected);
        let next = match (at, down) {
            (None, _) => 0,
            (Some(i), true) => (i + 1).min(prs.len() - 1),
            (Some(i), false) => i.saturating_sub(1),
        };
        let n = prs[next].number;
        self.select(n, cx);
    }

    /// The description of `pr`, parsed once.
    fn doc(&mut self, pr: &Pr) -> &Doc {
        self.docs
            .entry(pr.number)
            .or_insert_with(|| markdown::parse(&pr.body))
    }

    /// Fetches the demo `name` and opens it in the lightbox.
    fn watch(&mut self, name: String, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        self.busy = Some("loading the demo");
        let sent = self.send(Req::PrVideo { name: name.clone() }, cx);
        self.action = Some(cx.spawn(async move |this, cx| {
            let answer = sent.await;
            this.update(cx, |t, cx| {
                t.busy = None;
                match answer {
                    Ok(Resp::Blob(bytes)) => t.play(&name, &bytes, cx),
                    Ok(other) => t.say(format!("unexpected answer: {other:?}"), true),
                    Err(why) => t.say(format!("can't load {name}: {}", explain(&why)), true),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// A GIF opens as a picture, anything else in the player.
    fn play(&mut self, name: &str, bytes: &[u8], cx: &mut Context<Self>) {
        if let Some(format) = crate::transcript::attach::image_format(name, bytes) {
            let image = Arc::new(gpui::Image::from_bytes(format, bytes.to_vec()));
            Lightbox::show(name, image, cx);
            return;
        }
        if !video::is_video(name) {
            self.say(format!("can't play {name}: not a video"), true);
            return;
        }
        match Clip::from_bytes(name, bytes) {
            Ok(clip) => Lightbox::play(name, clip, cx),
            Err(why) => self.say(format!("can't play {name}: {why}"), true),
        }
    }

    fn say(&mut self, text: String, bad: bool) {
        self.notice = Some(Notice { text, bad });
    }

    /// The first click on Merge or Close asks; the second (on the confirm row) acts.
    fn ask(&mut self, what: Confirm, cx: &mut Context<Self>) {
        if self.busy.is_none() {
            self.confirm = Some(what);
            cx.notify();
        }
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        self.confirm = None;
        cx.notify();
    }

    /// Does what the confirm row asked.
    fn confirmed(&mut self, cx: &mut Context<Self>) {
        let Some(what) = self.confirm.take() else {
            return;
        };
        if self.busy.is_some() {
            return;
        }
        match what {
            Confirm::Merge(n) => self.merge(n, cx),
            Confirm::Close(n) => self.close_pr(n, cx),
        }
        cx.notify();
    }

    /// Notes the commit the box runs and merges; the list read after it says whether to watch the
    /// box for the new commit or the queued merge.
    fn merge(&mut self, number: u64, cx: &mut Context<Self>) {
        self.busy = Some("merging");
        self.notice = None;
        let info = self.send(Req::ServerInfo, cx);
        self.action = Some(cx.spawn(async move |this, cx| {
            let from = match info.await {
                Ok(Resp::ServerInfo { deployed, .. }) => deployed,
                _ => None,
            };
            let Ok(merged) = this.update(cx, |t, cx| t.send(Req::PrMerge { number }, cx)) else {
                return;
            };
            let answer = merged.await;
            this.update(cx, |t, cx| {
                t.busy = None;
                match answer {
                    // Merged now or queued behind a conflict fix: the list says which.
                    Ok(_) => {
                        t.pending = Some((number, from));
                        t.refresh(cx);
                    }
                    Err(why) => {
                        t.say(merge_failure(number, &why), true);
                        if is_conflict(&why) {
                            // Show "Fix with Claude" now; the read below confirms it.
                            if let Load::Ready(prs) = &mut t.list {
                                if let Some(pr) = prs.iter_mut().find(|p| p.number == number) {
                                    pr.mergeable = Some(false);
                                }
                            }
                            t.refresh(cx);
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn close_pr(&mut self, number: u64, cx: &mut Context<Self>) {
        self.busy = Some("closing");
        self.notice = None;
        let sent = self.send(Req::PrClose { number }, cx);
        self.action = Some(cx.spawn(async move |this, cx| {
            let answer = sent.await;
            this.update(cx, |t, cx| {
                t.busy = None;
                match answer {
                    Ok(_) => {
                        t.say(format!("#{number} closed"), false);
                        t.refresh(cx);
                    }
                    Err(why) => t.say(format!("#{number} not closed: {}", explain(&why)), true),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Asks the box every [`POLL`] which commit it runs, until it is a new one or [`POLL_FOR`] is up.
    fn watch_deploy(&mut self, cx: &mut Context<Self>) {
        let rounds = (POLL_FOR.as_secs() / POLL.as_secs()) as u32;
        self.poll = Some(cx.spawn(async move |this, cx| {
            for _ in 0..rounds {
                cx.background_executor().timer(POLL).await;
                let Ok(info) = this.update(cx, |t, cx| t.send(Req::ServerInfo, cx)) else {
                    return;
                };
                let now = match info.await {
                    Ok(Resp::ServerInfo { deployed, .. }) => deployed,
                    // The daemon restarts as the deploy lands: a lost answer is not the end.
                    _ => None,
                };
                let done = this
                    .update(cx, |t, cx| {
                        let Some(Deploy::Waiting {
                            number,
                            from,
                            polls,
                        }) = &mut t.deploy
                        else {
                            return true;
                        };
                        *polls += 1;
                        let number = *number;
                        let done = landed(from.as_deref(), now.as_deref());
                        if let (true, Some(sha)) = (done, now) {
                            t.deploy = Some(Deploy::Done { number, sha });
                        }
                        cx.notify();
                        done
                    })
                    .unwrap_or(true);
                if done {
                    return;
                }
            }
            this.update(cx, |t, cx| {
                if let Some(Deploy::Waiting { number, .. }) = t.deploy {
                    t.deploy = Some(Deploy::GaveUp { number });
                    cx.notify();
                }
            })
            .ok();
        }));
    }
}

impl Render for PrsOverlay {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The layer fades and the card rises into place; both go the same way, drawn until gone.
        let Some(a) = crate::motion::appear("prs-layer", self.open, window, cx) else {
            return div().into_any_element();
        };
        if std::mem::take(&mut self.want_focus) {
            cx.defer_in(window, |t, window, cx| {
                if t.restore.is_none() {
                    t.restore = window.focused(cx).filter(|h| !t.focus.contains(h, window));
                }
                window.focus(&t.focus, cx);
            });
        }
        self.layer(window.viewport_size(), a, cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_deploy_lands_when_the_box_names_another_commit() {
        assert!(landed(Some("aaa"), Some("bbb")));
        assert!(landed(None, Some("bbb")));
        assert!(!landed(Some("aaa"), Some("aaa")));
        // No answer, or no deployed file: still waiting.
        assert!(!landed(Some("aaa"), None));
        assert!(!landed(None, None));
    }

    #[test]
    fn the_deploy_line_counts_the_wait_and_shortens_the_commit() {
        let w = |polls| Deploy::Waiting {
            number: 7,
            from: None,
            polls,
        };
        assert_eq!(w(0).line(), ("#7 merged · deploying… 0s".into(), true));
        assert_eq!(w(25).line(), ("#7 merged · deploying… 2m 05s".into(), true));
        let done = Deploy::Done {
            number: 7,
            sha: "8b23d2d1f00".into(),
        };
        assert_eq!(done.line(), ("#7 deployed 8b23d2d".into(), true));
        assert!(!Deploy::GaveUp { number: 7 }.line().1);
    }

    fn pr(mergeable: Option<bool>, fixing: Option<Sid>) -> Pr {
        Pr {
            number: 43,
            title: "t".into(),
            branch: "claude-update/x".into(),
            author: "a".into(),
            url: String::new(),
            body: String::new(),
            additions: 1,
            deletions: 1,
            video: None,
            mergeable,
            fixing,
            merging: false,
        }
    }

    fn merging(fixing: Option<Sid>) -> Pr {
        Pr {
            merging: true,
            ..pr(Some(false), fixing)
        }
    }

    #[test]
    fn merge_is_always_the_main_button_and_a_conflict_adds_a_fix_on_the_side() {
        let sid = Sid::new();
        let a = |merge, fix| Actions { merge, fix };
        assert_eq!(actions(&pr(Some(true), None)), a(true, None));
        assert_eq!(actions(&pr(None, None)), a(true, None));
        // Conflicting: still merge (the daemon queues it behind a fix), "Fix with Claude" beside.
        assert_eq!(
            actions(&pr(Some(false), None)),
            a(true, Some(FixButton::Start))
        );
        assert_eq!(
            actions(&pr(Some(false), Some(sid))),
            a(true, Some(FixButton::Open(sid)))
        );
        // The fix pushed and GitHub says it merges: its chat is still a click away.
        assert_eq!(
            actions(&pr(Some(true), Some(sid))),
            a(true, Some(FixButton::Open(sid)))
        );
        // A queued merge runs: nothing to merge or start, its chat still opens.
        assert_eq!(actions(&merging(None)), a(false, None));
        assert_eq!(
            actions(&merging(Some(sid))),
            a(false, Some(FixButton::Open(sid)))
        );
        assert!(merge_waits(&pr(Some(false), None)));
        assert!(!merge_waits(&pr(Some(true), None)));
        assert!(!merge_waits(&merging(None)));
    }

    #[test]
    fn an_ok_merge_is_merged_unless_the_list_says_it_is_merging() {
        let other = Pr {
            number: 44,
            ..pr(Some(true), None)
        };
        assert_eq!(merge_ok(std::slice::from_ref(&other), 43), MergeOk::Merged);
        assert_eq!(merge_ok(&[pr(Some(true), None)], 43), MergeOk::Merged);
        assert_eq!(
            merge_ok(&[other.clone(), merging(None)], 43),
            MergeOk::Queued
        );
        assert_eq!(queued_outcome(&[other], 43), Some(true));
        assert_eq!(queued_outcome(&[merging(None)], 43), None);
        assert_eq!(queued_outcome(&[pr(Some(false), None)], 43), Some(false));
    }

    #[test]
    fn rows_mark_conflicts_fixes_and_unknowns() {
        let sid = Sid::new();
        assert_eq!(badge(&pr(Some(true), None)), None);
        assert_eq!(badge(&pr(Some(false), None)), Some(Badge::Conflicts));
        assert_eq!(badge(&pr(None, None)), Some(Badge::Checking));
        assert_eq!(badge(&pr(Some(false), Some(sid))), Some(Badge::Fixing));
        assert_eq!(badge(&merging(Some(sid))), Some(Badge::Merging));
        assert!(unsettled(&[pr(Some(true), None), merging(None)]));
        assert!(!unsettled(&[pr(Some(true), None), pr(Some(false), None)]));
        assert!(unsettled(&[pr(Some(true), None), pr(None, None)]));
        assert!(unsettled(&[pr(Some(false), Some(sid))]));
    }

    #[test]
    fn a_conflicting_merge_says_so_plainly() {
        let why =
            "Internal: gh pr merge: GraphQL: Pull Request has merge conflicts (mergePullRequest)";
        assert_eq!(
            merge_failure(43, why),
            "#43 conflicts with main — Fix with Claude"
        );
        assert_eq!(
            merge_failure(43, "Internal: gh: not logged in"),
            "#43 not merged: Internal: gh: not logged in"
        );
    }

    #[test]
    fn an_old_daemon_is_said_to_be_too_old() {
        assert!(explain("Unsupported: unknown request").starts_with("daemon too old"));
        assert_eq!(explain("gh: not logged in"), "gh: not logged in");
        assert_eq!(short("abc"), "abc");
    }
}
