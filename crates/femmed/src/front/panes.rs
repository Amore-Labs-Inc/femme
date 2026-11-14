//! What the desktop needs of a session's holder that `session.rs` does not offer: a control
//! connection kept for the next caller, and shell panes next to the agent's. A new file with an
//! `impl Daemon`, so `session.rs` stays as it is.

use std::io::ErrorKind;
use std::sync::{Arc, PoisonError};
use std::time::Duration;

use femme_proto::state::{Pane, PaneKind};
use femme_proto::{PaneId, Sid};
use femme_store::sessions;

use crate::core::CoreError;
use crate::daemon::{core_err, Daemon};
use crate::holder_client::{Error as HolderError, Holder};
use crate::session::{find_on_path, holder_err, COLS, MAIN, ROWS};

/// How long a holder may take to answer `Hello` before the desktop is told it is not there.
const HOLDER_WAIT: Duration = Duration::from_secs(5);

impl Daemon {
    /// A control connection to the session's holder. Kept per session: `Holder::connect` leaves
    /// its socket open until the holder exits, so a connect per terminal stream would pile them up.
    /// `NotFound` when the session has no holder (it ended, or the box restarted).
    pub(crate) async fn holder(&self, sid: Sid) -> Result<Holder, CoreError> {
        let slot = {
            let mut slots = self.holders.lock().unwrap_or_else(PoisonError::into_inner);
            // Slots nobody is using and that hold no live holder go as new ones come.
            slots.retain(|_, s| {
                Arc::strong_count(s) > 1
                    || s.try_lock()
                        .is_ok_and(|h| h.as_ref().is_some_and(Holder::is_alive))
            });
            slots.entry(sid).or_default().clone()
        };
        // Held across the connect, so three panes opened at once on a reconnect make one
        // connection; it is this session's alone, so a holder that does not answer holds up its
        // own callers (for `HOLDER_WAIT`) and nobody else's.
        let mut kept = slot.lock().await;
        if let Some(h) = kept.as_ref().filter(|h| h.is_alive()) {
            return Ok(h.clone());
        }
        let h = match tokio::time::timeout(HOLDER_WAIT, Holder::connect(&self.sessions.dirs, sid))
            .await
        {
            Ok(Ok(h)) => h,
            Ok(Err(HolderError::Io(e)))
                if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::ConnectionRefused) =>
            {
                return Err(CoreError::NotFound)
            }
            Ok(Err(e)) => return Err(holder_err(e)),
            Err(_) => return Err(CoreError::Internal("holder: no answer".into())),
        };
        *kept = Some(h.clone());
        Ok(h)
    }

    /// A new shell (`$SHELL`) in the session's holder, in the session's folder, as the next pane
    /// number; its row goes out like any change.
    // ponytail: two panes opened at the same instant can pick the same number; the second fails
    // in the holder and the desktop asks again. Serialize per session if that ever shows.
    pub async fn open_pane(&self, sid: Sid) -> Result<PaneId, CoreError> {
        let s = self.session(sid)?;
        let holder = match self.holder(sid).await {
            Ok(h) => h,
            // No holder: a herdr pane's chat (mirrored, its agent runs under herdr), or a session
            // whose agent ended. One is started for its shells alone; `reconcile` leaves a herdr
            // session's holder be and `end_session` shuts it down with the session.
            Err(CoreError::NotFound) => {
                self.sessions.spawn_holder(sid).await.map_err(holder_err)?;
                self.holder(sid).await?
            }
            Err(e) => return Err(e),
        };
        // The handle's own `info` is from when it connected; ask again for the panes made since.
        let live = holder.info().await.map_err(holder_err)?;
        let known = self
            .store
            .read(|c| sessions::panes(c, sid))
            .map_err(core_err)?;
        let no = live
            .panes
            .iter()
            .map(|p| p.no)
            .chain(known.iter().map(|p| p.id.no))
            .max()
            .unwrap_or(MAIN)
            .saturating_add(1);
        let path = self.sessions.path(&self.cfg);
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into());
        let prog = find_on_path(&shell, &path)
            .ok_or_else(|| CoreError::Internal(format!("{shell}: not found on PATH")))?;
        let env = self.sessions.child_env(&self.cfg, &path);
        holder
            .spawn_pty(no, vec![prog], s.cwd, env, COLS, ROWS)
            .await
            .map_err(holder_err)?;
        let id = PaneId { sid, no };
        let row = Pane {
            id,
            rev: 0,
            kind: PaneKind::Shell,
            title: String::new(),
        };
        if let Err(e) = self.write(|tx, _| tx.upsert_pane(&row)) {
            let _ = holder.close_pane(no).await; // no row, no shell
            return Err(e);
        }
        Ok(id)
    }

    /// Closes a pane the desktop opened: the shell gets SIGHUP, the row is deleted. The agent's own
    /// pane is the session's: closing it is `SessionClose`.
    pub async fn close_pane(&self, pane: PaneId) -> Result<(), CoreError> {
        if pane.no == MAIN {
            return Err(CoreError::Bad(
                "pane 1 is the session's own: close the session instead".into(),
            ));
        }
        self.session(pane.sid)?;
        let known = self
            .store
            .read(|c| sessions::get_pane(c, pane))
            .map_err(core_err)?;
        if known.is_none() {
            return Err(CoreError::NotFound);
        }
        match self.holder(pane.sid).await {
            Ok(holder) => match holder.close_pane(pane.no).await {
                // "no pane": its shell already left the holder; the row is all that remains.
                Ok(()) | Err(HolderError::Holder(_)) => {}
                Err(e) => return Err(holder_err(e)),
            },
            // The holder is gone (a crashed session): its shells went with it, only the row is left.
            Err(CoreError::NotFound) => {}
            Err(e) => return Err(e),
        }
        // The holder's `ChildExited` makes `follow` delete the row too, and it may get there first.
        match self.write(|tx, _| tx.delete_pane(pane)) {
            Err(CoreError::NotFound) => Ok(()),
            done => done,
        }
    }
}
