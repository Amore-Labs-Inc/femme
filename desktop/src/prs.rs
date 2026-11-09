//! The pull requests overlay (femme ▸ Pull requests): the box repo's open pull requests as a card
//! over the window. The list on the left (number, title, branch, +adds/−dels, author), the selected
//! one on the right: its title, its link, its description as Markdown, "Watch demo" when the branch
//! has a demo video, and "Merge & deploy" / "Close PR", each asked twice (merging ships to prod).
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
        self.refresh(cx);
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
                    Ok(Resp::Prs(prs)) => t.take(prs),
                    Ok(other) => t.fail(format!("unexpected answer: {other:?}")),
                    Err(why) => t.fail(explain(&why)),
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

    /// Notes the commit the box runs, merges, and watches the box for the new one.
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
                    Ok(_) => {
                        t.deploy = Some(Deploy::Waiting {
                            number,
                            from,
                            polls: 0,
                        });
                        t.watch_deploy(cx);
                        t.refresh(cx);
                    }
                    Err(why) => t.say(format!("#{number} not merged: {}", explain(&why)), true),
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

    #[test]
    fn an_old_daemon_is_said_to_be_too_old() {
        assert!(explain("Unsupported: unknown request").starts_with("daemon too old"));
        assert_eq!(explain("gh: not logged in"), "gh: not logged in");
        assert_eq!(short("abc"), "abc");
    }
}
