//! Which session the centre shows, and how the app opens one at start: the newest live Claude
//! session, else a new headless Claude session in the home directory, else (no `claude` on the
//! box) a shell. Owned by the sessions agent (docs/desktop-ui.md §sessions), together with
//! `net.rs`.
#![allow(missing_docs)] // stub: the owning agent documents what it fills in

use gpui::{Context, Entity};

use crate::net::Net;

/// Called once by the shell after the first `Connected`: picks or creates the session to show and
/// sets it as current on `Net`.
pub fn open_at_start(_net: &Entity<Net>, _cx: &mut Context<Net>) {
    // stub
}
