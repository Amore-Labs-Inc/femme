//! The input box under the transcript: what the user types goes to the session as a prompt
//! (`Req::Prompt`), attached images and files are uploaded first (`Req::BlobPut`) and shown as
//! hoverable thumbnails. Owned by the composer agent (docs/desktop-ui.md §composer).
#![allow(missing_docs)] // stub: the owning agent documents what it fills in

#[allow(dead_code)]
mod draft;
#[allow(dead_code)]
mod files;
#[allow(dead_code)]
mod sniff;

use femme_proto::Sid;
use gpui::{
    div, Context, Entity, FocusHandle, Focusable, IntoElement, ParentElement, Render, Window,
};

use crate::net::Net;

pub struct Composer {
    #[allow(dead_code)]
    net: Option<Entity<Net>>,
    session: Option<Sid>,
    focus: FocusHandle,
}

impl Composer {
    pub fn new(net: Option<Entity<Net>>, cx: &mut Context<Self>) -> Self {
        Composer {
            net,
            session: None,
            focus: cx.focus_handle(),
        }
    }

    pub fn set_session(&mut self, sid: Option<Sid>, cx: &mut Context<Self>) {
        self.session = sid;
        cx.notify();
    }
}

impl Focusable for Composer {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for Composer {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().child("composer (stub)")
    }
}
