//! What the desktop needs of a session's holder that `session.rs` does not offer: a control
//! connection kept for the next caller, and shell panes next to the agent's. A new file with an
//! `impl Daemon`, so `session.rs` stays as it is.

use std::io::ErrorKind;
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
        // Held across the connect: three panes opened at once on a reconnect make one connection.
        let mut kept = self.holders.lock().await;
        if let Some(h) = kept.get(&sid).filter(|h| h.is_alive()) {
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
        kept.retain(|_, h| h.is_alive());
        kept.insert(sid, h.clone());
        Ok(h)
    }

    /// A new shell (`$SHELL`) in the session's holder, in the session's folder, as the next pane
    /// number; its row goes out like any change.
    // ponytail: two panes opened at the same instant can pick the same number; the second fails
    // in the holder and the desktop asks again. Serialize per session if that ever shows.
    pub async fn open_pane(&self, sid: Sid) -> Result<PaneId, CoreError> {
        let s = self.session(sid)?;
        let holder = self.holder(sid).await?;
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
        let holder = self.holder(pane.sid).await?;
        match holder.close_pane(pane.no).await {
            // "no pane": its shell already left the holder; the row is all that remains.
            Ok(()) | Err(HolderError::Holder(_)) => {}
            Err(e) => return Err(holder_err(e)),
        }
        self.write(|tx, _| tx.delete_pane(pane))
    }
}
