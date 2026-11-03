//! The accounts overlay (femme ▸ accounts): every Claude account on the box with its 5 h / 7 d
//! usage, the active one, the per-account rotation caps (editable), AWS, and the API keys with
//! revoke / connect GitHub / add. Data comes through `Req::BoardApi` (the web app's JSON) and
//! `Req::Caps`. Owned by the accounts agent (docs/desktop-ui.md §accounts).
#![allow(missing_docs)] // stub: the owning agent documents what it fills in

use gpui::{div, Context, Entity, IntoElement, ParentElement, Render, Window};

use crate::net::Net;

pub struct AccountsOverlay {
    #[allow(dead_code)]
    net: Option<Entity<Net>>,
    open: bool,
}

impl AccountsOverlay {
    pub fn new(net: Option<Entity<Net>>, _cx: &mut Context<Self>) -> Self {
        AccountsOverlay { net, open: false }
    }

    pub fn open(&mut self, cx: &mut Context<Self>) {
        self.open = true;
        cx.notify();
    }

    pub fn close(&mut self, cx: &mut Context<Self>) {
        self.open = false;
        cx.notify();
    }

    pub fn is_open(&self) -> bool {
        self.open
    }
}

impl Render for AccountsOverlay {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().child(if self.open { "accounts (stub)" } else { "" })
    }
}
