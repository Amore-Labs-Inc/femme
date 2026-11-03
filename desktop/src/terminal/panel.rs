//! The terminals: one [`TerminalPane`] per pane of the live sessions, attached as the model
//! lists them. Rendered in the right sidebar's "terminal" tab (tabs, `+` for a shell, close) and,
//! for a shell session, in the centre through [`TerminalPanel::main_pane`]. Owned by the terminal
//! agent (docs/desktop-ui.md §terminal).
#![allow(missing_docs)] // stub: the owning agent documents what it fills in

use femme_proto::{PaneId, Sid};
use gpui::{div, Context, Entity, IntoElement, ParentElement, Render, Window};

use crate::net::Net;
use crate::terminal::pane::TerminalPane;

pub struct TerminalPanel {
    #[allow(dead_code)]
    net: Option<Entity<Net>>,
    session: Option<Sid>,
}

impl TerminalPanel {
    pub fn new(net: Option<Entity<Net>>, _cx: &mut Context<Self>) -> Self {
        TerminalPanel { net, session: None }
    }

    /// The session whose panes the tab shows.
    pub fn set_session(&mut self, sid: Option<Sid>, cx: &mut Context<Self>) {
        self.session = sid;
        cx.notify();
    }

    /// The attached pane, if it is.
    pub fn pane(&self, _id: PaneId) -> Option<Entity<TerminalPane>> {
        None // stub
    }

    /// The first pane of a session: what the centre shows for a shell session.
    pub fn main_pane(&self, _sid: Sid) -> Option<Entity<TerminalPane>> {
        None // stub
    }
}

impl Render for TerminalPanel {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().child("terminals (stub)")
    }
}
