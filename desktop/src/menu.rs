//! The `femme` button at the top left: a hoverable dropdown (version, check for updates, accounts,
//! reconnect, quit). Owned by the menu agent (docs/desktop-ui.md §menu); the shell mounts it.
#![allow(missing_docs)] // stub: the owning agent documents what it fills in

use gpui::{div, Context, Entity, EventEmitter, IntoElement, ParentElement, Render, Window};

use crate::net::Net;

/// What the menu asks the shell to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuEvent {
    /// Open the accounts overlay.
    Accounts,
    /// Compare this build with the box's deployed commit and say what to do.
    CheckUpdates,
    /// Drop the connection and connect again.
    Reconnect,
    /// Close the app.
    Quit,
}

pub struct FemmeMenu {
    #[allow(dead_code)]
    net: Option<Entity<Net>>,
}

impl FemmeMenu {
    pub fn new(net: Option<Entity<Net>>, _cx: &mut Context<Self>) -> Self {
        FemmeMenu { net }
    }
}

impl EventEmitter<MenuEvent> for FemmeMenu {}

impl Render for FemmeMenu {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().child("femme") // stub
    }
}
