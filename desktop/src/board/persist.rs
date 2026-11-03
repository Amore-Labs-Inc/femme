//! Keeping the drawing: a save half a second after the last change, written off the UI thread, and
//! the line of feedback under the canvas.

use std::time::Duration;

use gpui::{Context, SharedString};

use super::{Board, Notice};

/// How long after the last change the drawing is written.
const SAVE_DELAY: Duration = Duration::from_millis(500);

/// How long a line of feedback stays.
const NOTICE_TIME: Duration = Duration::from_secs(5);

impl Board {
    /// The editor may have changed: save later if the drawing did, and draw again.
    pub(super) fn changed(&mut self, cx: &mut Context<Self>) {
        let edits = self.editor.edits();
        if edits != self.seen_edits {
            self.seen_edits = edits;
            self.dirty = true;
            self.schedule_save(cx);
        }
        cx.notify();
    }

    /// Starts the half second before a save; a change before it ends starts it over.
    fn schedule_save(&mut self, cx: &mut Context<Self>) {
        self.save = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SAVE_DELAY).await;
            this.update(cx, |b, cx| b.save_now(cx)).ok();
        }));
    }

    /// Writes the drawing now if it changed since it was last written. The JSON is made here, the
    /// file is written on a background thread, and only one write is in flight at a time, so an
    /// older drawing can never land on top of a newer one.
    pub(super) fn save_now(&mut self, cx: &mut Context<Self>) {
        self.save = None;
        if !self.dirty {
            return;
        }
        if self.writing {
            self.rewrite = true;
            return;
        }
        self.writing = true;
        self.dirty = false;
        let json = self.editor.scene().to_json();
        let store = self.store.clone();
        cx.spawn(async move |this, cx| {
            let done = cx
                .background_executor()
                .spawn(async move { store.save_json(&json) })
                .await;
            this.update(cx, |b, cx| {
                b.writing = false;
                if let Err(e) = done {
                    b.dirty = true;
                    b.say(format!("could not save the board: {e}"), true, cx);
                }
                if std::mem::take(&mut b.rewrite) {
                    b.save_now(cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// Shows `text` under the canvas for a few seconds.
    pub(super) fn say(&mut self, text: impl Into<SharedString>, bad: bool, cx: &mut Context<Self>) {
        self.notice = Some(Notice {
            text: text.into(),
            bad,
        });
        self.notice_timer = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(NOTICE_TIME).await;
            this.update(cx, |b, cx| {
                b.notice = None;
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }
}
