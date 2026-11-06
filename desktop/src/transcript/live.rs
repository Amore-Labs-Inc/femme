//! The transcript of a session as the daemon streams it: `Item` rows (`Changes` on the session's
//! scope) and `Delta` text for the item in progress, drawn as a list that stays pinned to its end
//! while text arrives. Older history comes a page at a time when the user scrolls to the top; long
//! items, attached pictures and answers to questions go to the daemon as requests.
//!
//! The data is [`Items`]; which rows the list shows is [`turns::layout`]; what the list has to be
//! told when they change is [`splice::plan`]. This file keeps them together and talks to the
//! daemon.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use femme_client::{Client, Event, RequestError};
use femme_proto::rpc::{Req, Resp, Scope};
use femme_proto::state::{ItemStatus, SessionStatus};
use femme_proto::Sid;
use gpui::prelude::FluentBuilder as _;
use gpui::{
    div, list, px, relative, App, Context, Div, Entity, FocusHandle, Focusable, FollowMode,
    InteractiveElement as _, IntoElement, KeyDownEvent, ListAlignment, ListOffset, ListState,
    MouseButton, ParentElement as _, Pixels, Render, StatefulInteractiveElement as _, Styled as _,
    Subscription, Task, Window,
};
use gpui_base::animation::cubic_bezier;
use gpui_component::scroll::Scrollbar;
use gpui_component::WindowExt as _;
use tokio::sync::watch;

use super::agents::{self, Chip};
use super::ask::Ask;
use super::items::Items;
use super::markdown::{self, Doc};
use super::outbox::Sent;
use super::splice;
use super::thumb::Thumb;
use super::turns::{self, Key, Layout};
use super::working;
use crate::motion::appear;
use crate::net::{ClientEvent, Net};
use crate::theme::{
    rgba8, BAD, DONE, FONT_MONO, FONT_PIXEL, HOVER, INK, MUTED, NEON, PANEL, PAPER,
};
use crate::ui::{dot, meta, ruled};

/// How many older items one request asks for.
const PAGE: u16 = 100;

/// How long to wait before joining a scope again after the daemon refused.
const RETRY: Duration = Duration::from_secs(3);

/// How long the user has to be away from the end before the "↓" pill shows: a short scroll up
/// and back does not make it blink.
const PILL_AFTER: Duration = Duration::from_millis(150);

/// How long the scroll back to the end takes when the pill is clicked.
const GLIDE: Duration = Duration::from_millis(200);

/// The transcript of one session: build it once with [`Transcript::new`], point it at a session
/// with [`Transcript::set_session`].
pub struct Transcript {
    pub(super) net: Option<Entity<Net>>,
    pub(super) items: Items,
    /// The thread on show: the main one, or the subagent of this `Agent` call (see [`Items::view`]).
    pub(super) view: Option<String>,
    /// The subagents of the turn, for the strip under the list.
    chips: Vec<Chip>,
    /// The rows the list was last told about.
    pub(super) layout: Layout,
    /// The `seq`s the user opened: thinking, tool rows, fold lines.
    pub(super) open: HashSet<u64>,
    pub(super) list: ListState,
    focus: FocusHandle,
    /// The session whose scope should be subscribed; the task [`hold`] follows it.
    want: watch::Sender<Option<Sid>>,
    /// Counts the sessions shown, so answers to requests made for an earlier one are dropped.
    generation: u64,
    /// A page of older items is being fetched.
    pub(super) paging: bool,
    /// The last fetch of older items failed: the header offers it again instead of retrying.
    pub(super) page_failed: bool,
    fetching: HashSet<[u8; 32]>,
    failed: HashSet<[u8; 32]>,
    /// The attached files seen so far, by path.
    pub(super) thumbs: HashMap<String, Thumb>,
    /// The open questions.
    pub(super) asks: HashMap<u64, Ask>,
    /// The markdown of the messages, read once per version of the item.
    docs: HashMap<u64, (u64, Arc<Doc>)>,
    /// The rows of items made in the last second, and when each was first drawn: they fade in.
    pub(super) fresh: HashMap<u64, Instant>,
    /// Rows came in at the end while the user was reading further up, since when.
    unseen: Option<Instant>,
    /// Redraws once the pill's wait is over.
    pill_timer: Option<Task<()>>,
    /// The scroll back to the end under way: when it started and from how far down.
    glide: Option<(Instant, Pixels)>,
    /// The last request that failed, shown until it is dismissed.
    pub(super) note: Option<String>,
    /// Redraws once a second while the session works, for the clock in the working line.
    ticker: Option<Task<()>>,
    /// The working line the last row shows, `None` when the session is idle.
    pub(super) working: Option<String>,
    /// The prompts on their way that the last row shows ([`crate::net::Model::outbox`]).
    pub(super) sent: Vec<Sent>,
    /// The outbox's version when the transcript last looked.
    sent_ver: u64,
    /// Redraws when the oldest prompt on its way runs out.
    sent_timer: Option<Task<()>>,
    _events: Option<Subscription>,
    _status: Option<Subscription>,
}

impl Transcript {
    /// A transcript that shows nothing until [`set_session`](Self::set_session) names a session.
    /// Without a daemon (`net` is `None`) it stays empty.
    pub fn new(net: Option<Entity<Net>>, cx: &mut Context<Self>) -> Self {
        super::faces::register(cx);
        let (want, wanted) = watch::channel(None);
        if let Some(net) = &net {
            let (client, rt) = {
                let n = net.read(cx);
                (n.client(), n.rt())
            };
            rt.spawn(hold(client, wanted));
        }
        // One row past the items: the working line, empty when the session is idle.
        let list = ListState::new(1, ListAlignment::Top, px(300.));
        list.set_follow_mode(FollowMode::Tail);
        // The handler runs inside the list's own scroll code: it must not touch the list.
        let this = cx.weak_entity();
        list.set_scroll_handler(move |event, _, cx| {
            let (top, following) = (event.visible_range.start == 0, event.is_following_tail);
            this.update(cx, |t, cx| t.scrolled(top, following, cx)).ok();
        });
        let events = net.as_ref().map(|net| {
            cx.subscribe(net, |this, _, event: &ClientEvent, cx| {
                this.event(&event.0, cx);
            })
        });
        // The working line comes and goes with the session's status, which lives in the model.
        let status = net.as_ref().map(|net| {
            cx.observe(net, |this, net, cx| {
                let ver = net.read(cx).model.outbox.ver;
                if this.busy(cx) != this.ticker.is_some() || ver != this.sent_ver {
                    this.sent_ver = ver;
                    cx.notify();
                }
            })
        });
        Self {
            net,
            items: Items::new(None),
            view: None,
            chips: Vec::new(),
            layout: Layout::default(),
            open: HashSet::new(),
            list,
            focus: cx.focus_handle(),
            want,
            generation: 0,
            paging: false,
            page_failed: false,
            fetching: HashSet::new(),
            failed: HashSet::new(),
            thumbs: HashMap::new(),
            asks: HashMap::new(),
            docs: HashMap::new(),
            fresh: HashMap::new(),
            unseen: None,
            pill_timer: None,
            glide: None,
            note: None,
            ticker: None,
            working: None,
            sent: Vec::new(),
            sent_ver: 0,
            sent_timer: None,
            _events: events,
            _status: status,
        }
    }

    /// Shows the transcript of `sid` (`None`: nothing). The scope of the session shown before is
    /// left and the new one joined from its newest items.
    pub fn set_session(&mut self, sid: Option<Sid>, cx: &mut Context<Self>) {
        if self.items.sid() == sid {
            return;
        }
        self.items.switch(sid);
        self.view = None;
        self.chips.clear();
        self.layout = Layout::default();
        self.open.clear();
        self.asks.clear();
        self.docs.clear();
        self.fresh.clear();
        self.thumbs.clear();
        self.fetching.clear();
        self.failed.clear();
        (self.paging, self.page_failed) = (false, false);
        self.unseen = None;
        self.pill_timer = None;
        self.glide = None;
        self.note = None;
        self.generation += 1;
        self.list.reset(1);
        self.list.set_follow_mode(FollowMode::Tail);
        self.want.send_replace(sid);
        cx.notify();
    }

    /// What the daemon said: rows and text of the session shown.
    fn event(&mut self, event: &Event, cx: &mut Context<Self>) {
        let Some(mine) = self.items.sid() else {
            return;
        };
        let changed = match event {
            Event::Changes {
                scope: Scope::Session(sid),
                rows,
                ..
            } if *sid == mine => self.items.apply_rows(rows),
            Event::Delta {
                sid,
                item_id,
                offset,
                text,
            } if *sid == mine => self.items.apply_delta(item_id, *offset, text),
            Event::Reset {
                scope: Scope::Session(sid),
            } if *sid == mine => {
                self.items.reset();
                self.docs.clear();
                true
            }
            _ => false,
        };
        if changed {
            self.refresh(cx);
        }
    }

    /// Brings the list up to the items: the rows are worked out again, the list is told which
    /// were added, removed or changed.
    pub(super) fn refresh(&mut self, cx: &mut Context<Self>) {
        let next = turns::layout(&self.items, self.view.as_deref(), &self.open);
        let plan = splice::plan(&self.layout.rows, &next.rows);
        if let Some((old, count)) = plan.splice {
            self.list.splice(old, count);
        }
        for i in plan.remeasure {
            self.list.remeasure_items(i..i + 1);
        }
        if self.layout.rows.last() != next.rows.last()
            && !self.list.is_following_tail()
            && self.unseen.is_none()
        {
            self.unseen = Some(Instant::now());
            self.pill_timer = Some(cx.spawn(async move |this, cx| {
                cx.background_executor().timer(PILL_AFTER).await;
                this.update(cx, |_, cx| cx.notify()).ok();
            }));
        }
        self.layout = next;
        let items = &self.items;
        self.asks.retain(|seq, _| {
            items
                .get(*seq)
                .is_some_and(|e| e.item.status == ItemStatus::Pending)
        });
        cx.notify();
    }

    /// The list was scrolled by the user: to the top (older items are wanted), or back to the end.
    fn scrolled(&mut self, top: bool, following: bool, cx: &mut Context<Self>) {
        // The user's own scroll wins over the glide to the end.
        self.glide = None;
        if top {
            self.page_failed = false;
            self.load_older(cx);
        }
        if following && self.unseen.is_some() {
            self.seen();
            cx.notify();
        }
    }

    /// Shows one thread: the main one (`None`) or a subagent's. The list starts over at its end.
    pub(super) fn set_view(&mut self, view: Option<String>, cx: &mut Context<Self>) {
        if self.view == view {
            return;
        }
        self.view = view;
        self.layout = Layout::default();
        self.open.clear();
        self.fresh.clear();
        self.unseen = None;
        self.pill_timer = None;
        self.glide = None;
        self.list.reset(1);
        self.list.set_follow_mode(FollowMode::Tail);
        self.refresh(cx);
    }

    /// Back from a subagent to the main thread; false when that is what was showing.
    pub fn back(&mut self, cx: &mut Context<Self>) -> bool {
        if self.view.is_none() {
            return false;
        }
        self.set_view(None, cx);
        true
    }

    /// Opens or closes a thinking row, a tool row or a fold line.
    pub(super) fn toggle(&mut self, seq: u64, cx: &mut Context<Self>) {
        if !self.open.remove(&seq) {
            self.open.insert(seq);
        }
        self.refresh(cx);
    }

    /// The user is back at the end: the pill goes at once.
    fn seen(&mut self) {
        self.unseen = None;
        self.pill_timer = None;
    }

    /// Scrolls to the end and follows the text as it arrives again.
    fn follow(&mut self, cx: &mut Context<Self>) {
        self.glide = None;
        self.list.set_follow_mode(FollowMode::Tail);
        self.seen();
        cx.notify();
    }

    /// How far down the view is, and how far down the end is, as far as the list has measured.
    fn scroll_span(&self) -> (Pixels, Pixels) {
        (
            -self.list.scroll_px_offset_for_scrollbar().y,
            self.list.max_offset_for_scrollbar().y,
        )
    }

    /// Glides to the end over 200 ms, then follows the text again; a jump under "reduce motion".
    /// From far up it first jumps to a screen above the end, so the glide never sweeps through
    /// pages of rows.
    fn glide_to_end(&mut self, cx: &mut Context<Self>) {
        if cx.reduce_motion() {
            return self.follow(cx);
        }
        self.seen();
        let (now, end) = self.scroll_span();
        let screen = self.list.viewport_bounds().size.height;
        if end - now > screen * 2. {
            self.list.scroll_by(end - screen - now);
        }
        self.glide = Some((Instant::now(), self.scroll_span().0));
        cx.notify();
    }

    /// One frame of the glide, before the list is laid out. The end is read again every frame:
    /// rows below the view are measured only as they come into it, so it moves while the glide
    /// runs, and following the tail at the end lands exactly on it whatever was left.
    fn glide_step(&mut self, window: &mut Window) {
        let Some((start, from)) = self.glide else {
            return;
        };
        let t = start.elapsed().as_secs_f32() / crate::motion::scaled(GLIDE).as_secs_f32();
        if t >= 1. {
            self.glide = None;
            self.list.set_follow_mode(FollowMode::Tail);
            return;
        }
        let (now, end) = self.scroll_span();
        let ease = cubic_bezier(0.32, 0.72, 0., 1.);
        let want = from + (end - from) * ease(t);
        if want > now {
            self.list.scroll_by(want - now);
        }
        window.request_animation_frame();
    }

    /// The markdown of the message `seq` (whose text is `text`), read again only when the item
    /// changed.
    pub(super) fn doc(&mut self, seq: u64, text: &str) -> Arc<Doc> {
        let ver = self.items.get(seq).map_or(0, |e| e.ver);
        if let Some((v, doc)) = self.docs.get(&seq) {
            if *v == ver {
                return doc.clone();
            }
        }
        let doc = Arc::new(markdown::parse(text));
        self.docs.insert(seq, (ver, doc.clone()));
        doc
    }

    /// Runs `req` on the daemon and calls `done` with the answer, unless another session has been
    /// shown meanwhile.
    pub(super) fn request(
        &mut self,
        req: Req,
        cx: &mut Context<Self>,
        done: impl FnOnce(&mut Self, Result<Resp, RequestError>, &mut Context<Self>) + 'static,
    ) {
        let Some(net) = &self.net else {
            return;
        };
        let (client, rt) = {
            let n = net.read(cx);
            (n.client(), n.rt())
        };
        let (tx, rx) = tokio::sync::oneshot::channel();
        rt.spawn(async move {
            tx.send(client.request(req).await).ok();
        });
        let generation = self.generation;
        cx.spawn(async move |this, cx| {
            let Ok(answer) = rx.await else {
                return;
            };
            this.update(cx, |this, cx| {
                if this.generation == generation {
                    done(this, answer, cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// Asks for the page of items before the oldest one held.
    pub(super) fn load_older(&mut self, cx: &mut Context<Self>) {
        let (Some(sid), Some(before_seq)) = (self.items.sid(), self.items.older()) else {
            return;
        };
        if self.paging {
            return;
        }
        self.paging = true;
        self.page_failed = false;
        let req = Req::TranscriptPage {
            sid,
            before_seq,
            limit: PAGE,
        };
        self.request(req, cx, |this, answer, cx| {
            this.paging = false;
            match answer {
                Ok(Resp::Page { items, next_before }) => {
                    // The older items go above what the user was reading, which stays where it is.
                    let place = this.place();
                    this.items.apply_page(items, next_before);
                    this.refresh(cx);
                    this.restore(place);
                    return;
                }
                Ok(_) => {}
                Err(e) => this.note = Some(format!("older messages: {e}")),
            }
            this.page_failed = true;
            cx.notify();
        });
        cx.notify();
    }

    /// The row at the top of the view and how far the view starts into it; the first message
    /// when the view is on the "older messages" header. `None` while the view is at the end.
    fn place(&self) -> Option<(Key, Pixels)> {
        let top = self.list.logical_scroll_top();
        let rows = &self.layout.rows;
        let on_header = rows.get(top.item_ix).is_some_and(|r| r.key == Key::More);
        let (ix, offset) = if on_header {
            (top.item_ix + 1, px(0.))
        } else {
            (top.item_ix, top.offset_in_item)
        };
        rows.get(ix).map(|r| (r.key, offset))
    }

    /// Puts the view back on the row `place` named, wherever it is now.
    fn restore(&mut self, place: Option<(Key, Pixels)>) {
        let Some((key, offset)) = place else {
            return;
        };
        if let Some(ix) = self.layout.rows.iter().position(|r| r.key == key) {
            self.list.scroll_to(ListOffset {
                item_ix: ix,
                offset_in_item: offset,
            });
        }
    }

    /// Fetches the body of a long item (a blob) once.
    pub(super) fn fetch_blob(&mut self, hash: [u8; 32], cx: &mut Context<Self>) {
        if self.failed.contains(&hash) || !self.fetching.insert(hash) {
            return;
        }
        self.request(Req::BlobGet { hash }, cx, move |this, answer, cx| {
            this.fetching.remove(&hash);
            match answer {
                Ok(Resp::Blob(bytes)) => {
                    if this.items.set_blob(hash, &bytes) {
                        this.refresh(cx);
                    }
                }
                Ok(_) => {
                    this.failed.insert(hash);
                }
                Err(e) => {
                    this.failed.insert(hash);
                    this.note = Some(format!("a long item was not fetched: {e}"));
                    cx.notify();
                }
            }
        });
    }

    /// Whether one of `wants` is still on its way (a fetch that failed is not).
    pub(super) fn fetching_any(&self, wants: &[[u8; 32]]) -> bool {
        wants.iter().any(|h| !self.failed.contains(h))
    }

    /// Reads an attached picture from the box, once.
    pub(super) fn want_thumb(&mut self, path: &str, cx: &mut Context<Self>) {
        let Some(sid) = self.items.sid() else {
            return;
        };
        if self.thumbs.contains_key(path) {
            return;
        }
        self.thumbs.insert(path.to_owned(), Thumb::Loading);
        let path = path.to_owned();
        let req = Req::FsRead {
            sid,
            path: path.clone(),
        };
        self.request(req, cx, move |this, answer, cx| {
            let name = path.rsplit('/').next().unwrap_or(&path).to_owned();
            let thumb = match answer {
                Ok(Resp::FsData(bytes)) => Thumb::from_bytes(&name, bytes),
                Ok(_) => Thumb::Failed,
                Err(e) => {
                    this.note = Some(format!("{name}: {e}"));
                    Thumb::Failed
                }
            };
            this.thumbs.insert(path, thumb);
            cx.notify();
        });
    }

    fn on_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let page = self.list.viewport_bounds().size.height * 0.9;
        match event.keystroke.key.as_str() {
            "pageup" => self.list.scroll_by(-page),
            "pagedown" => self.list.scroll_by(page),
            "home" => self.list.scroll_to(ListOffset {
                item_ix: 0,
                offset_in_item: px(0.),
            }),
            "end" => {
                self.list.set_follow_mode(FollowMode::Tail);
                self.seen();
            }
            "escape" if self.view.is_some() => {
                self.back(cx);
                cx.stop_propagation();
                return;
            }
            _ => return,
        }
        self.glide = None;
        window.refresh();
        cx.notify();
    }

    /// The words over an empty transcript.
    fn hint(&self) -> Div {
        let word = if self.net.is_none() {
            "no daemon"
        } else if self.items.sid().is_none() {
            "no session"
        } else {
            "nothing here yet"
        };
        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .font_family(FONT_PIXEL)
            .text_size(px(22.))
            .text_color(MUTED)
            .child(word)
    }

    /// The pill that brings the user back to the end of the text, rising 6 px as it fades in.
    fn pill(&self, window: &mut Window, cx: &mut Context<Self>) -> Option<Div> {
        let shown = self.unseen.is_some_and(|t| t.elapsed() >= PILL_AFTER);
        // Keyed by the session shown, so a pill left over from the last session does not fade
        // out over the next one.
        let a = appear(("to-end", self.generation), shown, window, cx)?;
        Some(
            div()
                .absolute()
                .bottom(px(12.) - a.offset(6.))
                .opacity(a.opacity())
                .left_0()
                .right_0()
                .flex()
                .justify_center()
                .child(
                    ruled(
                        div()
                            .id("to-end")
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .px(px(10.))
                            .py(px(4.))
                            .bg(INK)
                            .text_color(PAPER)
                            .cursor_pointer()
                            .hover(|s| s.bg(HOVER)),
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.glide_to_end(cx)))
                    .child(div().font_family(FONT_MONO).text_size(px(12.)).child("↓"))
                    .child(
                        div()
                            .font_family(FONT_PIXEL)
                            .text_size(px(20.))
                            .line_height(relative(1.))
                            .child("new"),
                    ),
                ),
        )
    }
}

/// Keeps the daemon's subscription on the scope of the session `want` names: the scope held is
/// left before the next is joined, and a join the daemon refuses is tried again. Ends (leaving the
/// scope) when the transcript is dropped.
async fn hold(client: Arc<Client>, mut want: watch::Receiver<Option<Sid>>) {
    let mut held: Option<Sid> = None;
    loop {
        let target = *want.borrow_and_update();
        if held != target {
            if let Some(old) = held.take() {
                client.unsubscribe(Scope::Session(old)).await;
            }
            if let Some(sid) = target {
                if client.subscribe(Scope::Session(sid)).await.is_ok() {
                    held = Some(sid);
                } else {
                    tokio::select! {
                        changed = want.changed() => if changed.is_err() { break },
                        () = tokio::time::sleep(RETRY) => {}
                    }
                }
            }
            // The wish may have moved while the daemon was answering: read it again.
            continue;
        }
        if want.changed().await.is_err() {
            break;
        }
    }
    if let Some(old) = held {
        client.unsubscribe(Scope::Session(old)).await;
    }
}

impl Focusable for Transcript {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Transcript {
    /// The "✻ Thinking… (42s · ↓ 1.2k tokens)" line while the session shown works, and a ticker
    /// that redraws it every second; `None` (and no ticker) otherwise.
    fn busy(&self, cx: &App) -> bool {
        let (Some(sid), Some(net)) = (self.items.sid(), &self.net) else {
            return false;
        };
        net.read(cx)
            .model
            .sessions
            .get(&sid)
            .is_some_and(|s| s.status == SessionStatus::Working)
    }

    fn working_line(&mut self, cx: &mut Context<Self>) -> Option<String> {
        // A subagent's view works while its call has no result.
        let view_done = self
            .view
            .as_ref()
            .is_some_and(|v| !self.chips.iter().any(|c| c.running && c.id == *v));
        if !self.busy(cx) || view_done {
            self.ticker = None;
            return None;
        }
        if self.ticker.is_none() {
            self.ticker = Some(cx.spawn(async move |this, cx| loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    break;
                }
            }));
        }
        Some(working::line(
            &working::working(&self.items, self.view.as_deref()),
            now_ms(),
        ))
    }

    /// The strip under the list: the main thread first, then one chip per subagent of the turn;
    /// the thread on show is lit. Nothing while the turn has no subagents.
    fn strip(&self, cx: &mut Context<Self>) -> Option<Div> {
        if self.chips.is_empty() && self.view.is_none() {
            return None;
        }
        let now = now_ms();
        let tab = |id: &'static str, on: bool| {
            div()
                .id(id)
                .flex()
                .flex_none()
                .items_center()
                .gap(px(5.))
                .h(px(18.))
                .px(px(6.))
                .cursor_pointer()
                .hover(|s| s.bg(PANEL))
                .when(on, |d| d.bg(PANEL).text_color(INK))
        };
        let main = tab("agents-main", self.view.is_none())
            .child("‹ main")
            .on_click(cx.listener(|this, _, _, cx| {
                this.back(cx);
            }));
        let chips = self.chips.iter().enumerate().map(|(ix, c)| {
            let on = self.view.as_deref() == Some(c.id.as_str());
            let id = c.id.clone();
            let when = if c.running {
                agents::elapsed(c.since, now)
            } else {
                "done".to_owned()
            };
            div()
                .id(("agent", ix))
                .flex()
                .flex_none()
                .items_center()
                .gap(px(5.))
                .h(px(18.))
                .px(px(6.))
                .max_w(px(220.))
                .cursor_pointer()
                .hover(|s| s.bg(PANEL))
                .when(on, |d| d.bg(PANEL))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.set_view(Some(id.clone()), cx);
                }))
                .child(dot(if c.running { NEON } else { DONE }))
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_color(if c.running { INK } else { MUTED })
                        .child(c.label.clone()),
                )
                .child(div().flex_none().text_color(MUTED).child(when))
        });
        Some(
            div()
                .flex_none()
                .flex()
                .items_center()
                .gap(px(4.))
                .h(px(24.))
                .px(px(6.))
                .border_t_1()
                .border_color(rgba8(0x1E1E1E, 0.16))
                .bg(PAPER)
                .font_family(FONT_MONO)
                .text_size(px(11.))
                .text_color(MUTED)
                .overflow_hidden()
                .child(main)
                .children(chips),
        )
    }

    /// The prompts of the session shown that are on their way; a redraw is set for when the next
    /// one runs out, and the list follows the end again when one is added.
    fn sent_now(&mut self, cx: &mut Context<Self>) -> Vec<Sent> {
        let (Some(sid), Some(net)) = (self.items.sid(), self.net.clone()) else {
            return Vec::new();
        };
        let now = Instant::now();
        let outbox = &net.read(cx).model.outbox;
        let sent: Vec<Sent> = outbox.shown(sid, now).cloned().collect();
        let expiry = outbox.next_expiry(sid, now);
        if sent != self.sent {
            if sent.iter().any(|s| !self.sent.iter().any(|o| o.id == s.id)) {
                self.follow(cx);
            }
            self.sent_timer = expiry.map(|at| {
                cx.spawn(async move |this, cx| {
                    cx.background_executor()
                        .timer(at.saturating_duration_since(Instant::now()))
                        .await;
                    this.update(cx, |_, cx| cx.notify()).ok();
                })
            });
        }
        sent
    }
}

impl Render for Transcript {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.unseen.is_some() && self.list.is_following_tail() {
            self.seen();
        }
        if self.unseen.is_some() {
            // The list takes up following again while it is laid out, after this render: look
            // once more when the frame is done, so the pill goes when the user is back at the end.
            let this = cx.weak_entity();
            window.on_next_frame(move |_, cx| {
                this.update(cx, |t, cx| {
                    if t.list.is_following_tail() {
                        t.seen();
                        cx.notify();
                    }
                })
                .ok();
            });
        }
        self.glide_step(window);
        self.chips = agents::chips(&self.items);
        let working = self.working_line(cx);
        let sent = self.sent_now(cx);
        if working.is_some() != self.working.is_some() || sent != self.sent {
            let last = self.layout.rows.len();
            self.list.remeasure_items(last..last + 1);
        }
        self.working = working;
        self.sent = sent;
        let pill = self.pill(window, cx);
        let strip = self.strip(cx);
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(PAPER)
            .track_focus(&self.focus)
            // A click gives the transcript the keyboard unless a text field has it: a click on a
            // row must not take it from the composer. This listener runs after the fields in the
            // rows (a question's answer box takes the keyboard when the click reaches it) and
            // before gpui's own focus-on-click for this element, which it cancels.
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_, _, window, cx| {
                    if window.has_focused_input(cx) {
                        window.prevent_default();
                    }
                }),
            )
            .on_key_down(cx.listener(Self::on_key))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .relative()
                    .child(
                        list(
                            self.list.clone(),
                            cx.processor(|this, ix, window, cx| this.row(ix, window, cx)),
                        )
                        .size_full()
                        .pt(px(6.))
                        .pb(px(10.)),
                    )
                    .child(Scrollbar::vertical(&self.list))
                    .when(self.layout.rows.is_empty() && self.sent.is_empty(), |d| {
                        d.child(self.hint())
                    })
                    .children(pill)
                    .when_some(self.note.clone(), |d, note| {
                        d.child(
                            ruled(
                                div()
                                    .id("note")
                                    .absolute()
                                    .bottom(px(8.))
                                    .left(px(12.))
                                    .max_w(relative(0.8))
                                    .px(px(8.))
                                    .py(px(3.))
                                    .bg(PAPER)
                                    .cursor_pointer()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.note = None;
                                        cx.notify();
                                    })),
                            )
                            .child(meta(note).text_color(BAD)),
                        )
                    }),
            )
            .children(strip)
    }
}

/// Unix ms now.
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}
