//! The right sidebar's "files" tab: the session's directory on the box (`Req::FsList`), a click
//! reads the file (`Req::FsRead`) and shows it with [`crate::viewer::FileView`]. Owned by the files
//! agent (docs/desktop-ui.md §files).
#![allow(missing_docs)] // stub: the owning agent documents what it fills in

use femme_proto::Sid;
use gpui::{div, Context, Entity, IntoElement, ParentElement, Render, Window};

use crate::net::Net;

pub struct FilesPanel {
    #[allow(dead_code)]
    net: Option<Entity<Net>>,
    session: Option<Sid>,
    cwd: String,
}

impl FilesPanel {
    pub fn new(net: Option<Entity<Net>>, _cx: &mut Context<Self>) -> Self {
        FilesPanel {
            net,
            session: None,
            cwd: String::new(),
        }
    }

    /// The session whose directory is listed (`cwd` as the session row says it).
    pub fn set_session(&mut self, sid: Option<Sid>, cwd: &str, cx: &mut Context<Self>) {
        self.session = sid;
        self.cwd = cwd.to_string();
        cx.notify();
    }
}

impl Render for FilesPanel {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().child(format!("files of {} (stub)", self.cwd))
    }
}
