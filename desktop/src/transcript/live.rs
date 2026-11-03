//! The transcript of a session as the daemon streams it: `Item` rows (`Changes` on the session's
//! scope) and `Delta` text for the item in progress; markdown bodies, folded tool calls, question
//! rows the user can answer. Owned by the transcript agent (docs/desktop-ui.md §transcript), who
//! also deletes the synthetic transcript (`model.rs`, `pacer.rs`, `view.rs`, `rng.rs`).
#![allow(missing_docs)] // stub: the owning agent documents what it fills in

use femme_proto::Sid;
use gpui::{div, Context, Entity, IntoElement, ParentElement, Render, Window};

use crate::net::Net;

pub struct Transcript {
    #[allow(dead_code)]
    net: Option<Entity<Net>>,
    session: Option<Sid>,
}

impl Transcript {
    pub fn new(net: Option<Entity<Net>>, _cx: &mut Context<Self>) -> Self {
        Transcript { net, session: None }
    }

    pub fn set_session(&mut self, sid: Option<Sid>, cx: &mut Context<Self>) {
        self.session = sid;
        cx.notify();
    }
}

impl Render for Transcript {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().child("transcript (stub)")
    }
}
