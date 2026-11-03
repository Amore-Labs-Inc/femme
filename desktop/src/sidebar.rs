//! The left sidebar: the live sessions only, the current one marked; collapsible. A click emits
//! [`OpenSession`]. Owned by the sidebar agent (docs/desktop-ui.md §sidebar).
#![allow(missing_docs)] // stub: the owning agent documents what it fills in

use femme_proto::Sid;
use gpui::{div, Context, Entity, EventEmitter, IntoElement, ParentElement, Render, Window};

use crate::net::Net;

/// The user picked a session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OpenSession(pub Sid);

pub struct Sidebar {
    #[allow(dead_code)]
    net: Option<Entity<Net>>,
    selected: Option<Sid>,
}

impl Sidebar {
    pub fn new(net: Option<Entity<Net>>, _cx: &mut Context<Self>) -> Self {
        Sidebar {
            net,
            selected: None,
        }
    }

    pub fn set_selected(&mut self, sid: Option<Sid>, cx: &mut Context<Self>) {
        self.selected = sid;
        cx.notify();
    }
}

impl EventEmitter<OpenSession> for Sidebar {}

impl Render for Sidebar {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().child("sessions (stub)")
    }
}
