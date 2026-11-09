//! The daemon's [`Core`]: the store behind the compat layer, with every change fanned out through
//! the [`Hub`]; sessions run in holders ([`crate::session`]), headless agents are driven by an
//! actor per session ([`crate::headless`]), the scheduler loops in [`crate::worker`].

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use tokio::sync::Notify;

use async_trait::async_trait;
use femme_proto::rpc::{Answer, IssueField, PromptHow, Scope};
use femme_proto::state::{Agent, Issue, Item, Mode, Pane, Row, Session};
use femme_proto::Sid;
use femme_store::{auth, issues, sessions, Store};
use serde_json::Value;

use crate::config::Config;
use crate::core::{
    ApiKey, Core, CoreError, CreateSession, GithubAccount, PaneInfo, PaneRef, Screen, SessionInfo,
};
use crate::holder_client::Holder;
use crate::hub::{Backlog, Hub};
use crate::notify::Notifier;
use crate::scheduler::{self, UsageSnapshot};
use crate::session::Sessions;

/// One page of a subscriber's backlog (plan §6: ≤ 1 MiB inline).
const BACKLOG_BYTES: usize = 1 << 20;

pub struct Daemon {
    pub cfg: Config,
    pub store: Arc<Store>,
    pub hub: Hub,
    pub sessions: Sessions,
    pub notifier: Notifier,
    /// The scheduler's last `/usage` reading, shown by `/api/meta`.
    pub usage: Mutex<Option<UsageSnapshot>>,
    /// Rung when a session row changes: the scheduler plans again ([`crate::worker`]).
    pub sched_wake: Arc<Notify>,
    /// Control connections to holders, one per session, for the desktop front (`crate::front`):
    /// every `Holder::connect` leaves a socket open until its holder exits, so they are kept. A
    /// slot per session, so connecting to one holder holds up only that session's callers.
    pub(crate) holders: Mutex<HashMap<Sid, Arc<tokio::sync::Mutex<Option<Holder>>>>>,
    /// Ourselves, for the tasks that follow holders (they must not keep the daemon alive).
    pub(crate) me: Weak<Daemon>,
}

impl Daemon {
    /// Opens (or creates) `<state>/femme.db` and the hub over it, and writes the agents' hook
    /// configs. Holders are reattached by [`Daemon::reconcile`].
    pub fn open(cfg: Config) -> anyhow::Result<Arc<Daemon>> {
        std::fs::create_dir_all(&cfg.state_dir)?;
        let mut store = Store::open(&cfg.state_dir.join("femme.db"))?;
        recover_messages(&mut store, &cfg);
        let store = Arc::new(store);
        let hub = Hub::new(Arc::new({
            let store = store.clone();
            move |scope, after| backlog(&store, scope, after).map_err(|e| e.to_string())
        }));
        let sessions = Sessions::new(&cfg);
        sessions.write_hook_files()?;
        let notifier = Notifier::new(Some(cfg.ntfy_topic.clone()));
        Ok(Arc::new_cyclic(|me| Daemon {
            cfg,
            store,
            hub,
            sessions,
            notifier,
            usage: Mutex::new(None),
            sched_wake: Arc::default(),
            holders: Default::default(),
            me: me.clone(),
        }))
    }

    /// The old board's JSON files in the state dir, once (the import is idempotent).
    pub fn import_board(&self) -> anyhow::Result<femme_store::ImportReport> {
        Ok(self
            .store
            .import_board_json(&self.cfg.state_dir, now_ms())?)
    }

    /// The store's retention ([`Store::retain`]), once at start and then daily: what it drops is
    /// published like any change, the purged sessions' spools, snapshots and uploads go with them, and the
    /// phone hears when the database passes 1 GiB.
    pub async fn retain_daily(daemon: Weak<Daemon>) {
        loop {
            let Some(d) = daemon.upgrade() else { return };
            d.retain();
            drop(d);
            tokio::time::sleep(Duration::from_secs(24 * 3600)).await;
        }
    }

    fn retain(&self) {
        let before = self.store.rev().ok();
        match self.store.retain(now_ms()) {
            Ok(r) => {
                for sid in &r.purged_sessions {
                    let _ = std::fs::remove_file(self.sessions.dirs.spool(*sid));
                    let _ = std::fs::remove_file(self.sessions.dirs.snap(*sid));
                    self.forget_uploads(*sid);
                }
                tracing::info!(
                    sessions = r.sessions,
                    items = r.items,
                    tombstones = r.tombstones,
                    "retention"
                );
                match backlog(&self.store, Scope::Global, before) {
                    Ok(b) => self.hub.publish(b.rows),
                    Err(e) => tracing::warn!("changes after rev {before:?} not published: {e}"),
                }
            }
            Err(e) => tracing::warn!(%e, "retention failed"),
        }
        if let Ok(bytes) = self
            .store
            .db_size_bytes()
            .inspect_err(|e| tracing::warn!(%e, "database size unknown"))
        {
            if bytes > 1 << 30 {
                self.notifier.push(
                    "FEMME database over 1 GiB".into(),
                    format!(
                        "{} MiB after retention: time to look at what is kept",
                        bytes >> 20
                    ),
                    3,
                );
            }
        }
    }

    /// A write at `now`, then whatever it changed (rows and tombstones, by rev) goes to the hub.
    pub(crate) fn write<T>(
        &self,
        f: impl FnOnce(&mut femme_store::Tx<'_>, i64) -> femme_store::Result<T>,
    ) -> Result<T, CoreError> {
        self.write_items(|tx, now, _| f(tx, now))
    }

    /// [`Daemon::write`] for a transaction that also writes transcript items: the global change
    /// feed does not carry items, so the closure pushes each stored item it wants published into
    /// the vector, and they go to the hub after the commit, behind the session and pane rows.
    pub(crate) fn write_items<T>(
        &self,
        f: impl FnOnce(&mut femme_store::Tx<'_>, i64, &mut Vec<Item>) -> femme_store::Result<T>,
    ) -> Result<T, CoreError> {
        let before = self.store.rev().map_err(core_err)?;
        let now = now_ms();
        let mut items = Vec::new();
        let out = self
            .store
            .write_at(now, |tx| f(tx, now, &mut items))
            .map_err(core_err)?;
        match backlog(&self.store, Scope::Global, Some(before)) {
            Ok(b) => {
                if b.rows.iter().any(|r| matches!(r, Row::Session(_))) {
                    self.sched_wake.notify_one();
                }
                let mut rows = b.rows;
                rows.extend(items.into_iter().map(Row::Item));
                self.hub.publish(rows);
            }
            Err(e) => tracing::warn!("changes after rev {before} not published: {e}"),
        }
        Ok(out)
    }
}

/// A subscriber's catch-up: every page the byte budget split.
// ponytail: all pages are read into memory, then the hub chunks them; page them straight into the
// hub's sender (or `SnapshotOnBulk`) when a first subscribe outgrows memory.
fn backlog(store: &Store, scope: Scope, after: Option<u64>) -> femme_store::Result<Backlog> {
    let first = store.changes_since(scope, after, BACKLOG_BYTES)?;
    let (reset, mut rows, mut rev, mut more) = (first.reset, first.rows, first.rev, first.more);
    while more {
        let next = store.changes_since(scope, Some(rev), BACKLOG_BYTES)?;
        rows.extend(next.rows);
        (rev, more) = (next.rev, next.more);
    }
    Ok(Backlog { rows, rev, reset })
}

pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

pub(crate) fn core_err(e: femme_store::Error) -> CoreError {
    use femme_store::Error as E;
    match e {
        E::NotFound => CoreError::NotFound,
        E::Conflict { .. } | E::Exists(_) => CoreError::Conflict(e.to_string()),
        E::TooLarge { .. } => CoreError::Bad(e.to_string()),
        E::Sqlite(_) | E::Corrupt(_) | E::Io(_) | E::Json(_) => CoreError::Internal(e.to_string()),
    }
}

pub(crate) fn info(s: &Session, panes: Vec<Pane>) -> SessionInfo {
    SessionInfo {
        sid: s.id,
        handle: s.handle.clone(),
        machine: s.name.is_some().then(|| "instinct".to_string()),
        name: s.name.clone(),
        owner: s.owner.clone(),
        agent: s.agent,
        mode: s.mode,
        cwd: s.cwd.clone(),
        label: s.label.clone(),
        status: s.status,
        attention: s.attention,
        panes: panes
            .into_iter()
            .map(|p| PaneInfo {
                no: p.id.no,
                kind: p.kind,
                title: p.title,
                cwd: None,
            })
            .collect(),
    }
}

fn key_info(k: femme_store::ApiKey) -> ApiKey {
    ApiKey {
        name: k.name,
        key: k.key,
        github_token: k.github_token,
        github_login: k.github_login,
        git_name: k.git_name,
        git_email: k.git_email,
        created: k.created,
    }
}

#[async_trait]
impl Core for Daemon {
    async fn terminals(&self) -> Result<Vec<SessionInfo>, CoreError> {
        self.store
            .read(|c| {
                sessions::list_sessions(c, false)?
                    .iter()
                    .map(|s| Ok(info(s, sessions::panes(c, s.id)?)))
                    .collect()
            })
            .map_err(core_err)
    }

    async fn session_by_name(&self, name: &str) -> Result<Option<SessionInfo>, CoreError> {
        self.store
            .read(|c| {
                sessions::session_by_name(c, name)?
                    .map(|s| Ok(info(&s, sessions::panes(c, s.id)?)))
                    .transpose()
            })
            .map_err(core_err)
    }

    async fn create_session(&self, req: CreateSession) -> Result<SessionInfo, CoreError> {
        self.open_session(req).await
    }

    async fn close_session(&self, sid: Sid) -> Result<(), CoreError> {
        self.end_session(sid).await
    }

    async fn screen(&self, pane: &PaneRef, ansi: bool) -> Result<Screen, CoreError> {
        self.pane_screen(pane, ansi).await
    }

    async fn prompt(&self, pane: &PaneRef, text: &str) -> Result<(), CoreError> {
        self.pane_prompt(pane, text, PromptHow::Queue, None).await
    }

    async fn input(&self, pane: &PaneRef, bytes: &[u8]) -> Result<(), CoreError> {
        self.pane_input(pane, bytes).await
    }

    async fn interrupt(&self, sid: Sid) -> Result<(), CoreError> {
        self.interrupt_session(sid).await
    }

    async fn background(&self, sid: Sid, tool_use_id: Option<&str>) -> Result<(), CoreError> {
        self.background_task(sid, tool_use_id).await
    }

    async fn answer(&self, sid: Sid, request_id: &str, answer: Answer) -> Result<(), CoreError> {
        self.answer_question(sid, request_id, answer).await
    }

    async fn set_model(&self, sid: Sid, model: &str) -> Result<(), CoreError> {
        self.switch_model(sid, model).await
    }

    async fn takeover(&self, sid: Sid, to: Mode) -> Result<(), CoreError> {
        self.switch_mode(sid, to).await
    }

    async fn issues(&self) -> Result<Vec<Issue>, CoreError> {
        self.store.read(issues::list_issues).map_err(core_err)
    }

    async fn issue_create(&self, repo: &str, title: &str, body: &str) -> Result<Issue, CoreError> {
        self.write(|tx, now| tx.create_issue(repo, title, body, now))
    }

    async fn issue_patch(&self, id: u64, fields: Vec<IssueField>) -> Result<Issue, CoreError> {
        // The board had no base rev (last writer wins): patch against the current one in the same
        // transaction, so the per-field merge never refuses.
        self.write(|tx, now| {
            let cur = issues::get_issue(tx, id)?.ok_or(femme_store::Error::NotFound)?;
            tx.patch_issue(id, cur.rev, &fields, now)
        })
    }

    async fn issue_delete(&self, id: u64) -> Result<(), CoreError> {
        self.write(|tx, _| tx.delete_issue(id))
    }

    async fn web_session_valid(&self, token_sha256: [u8; 32]) -> bool {
        self.store
            .read(|c| auth::web_session_valid(c, &token_sha256, now_ms()))
            .unwrap_or(false)
    }

    async fn web_session_create(
        &self,
        token_sha256: [u8; 32],
        expires: i64,
    ) -> Result<(), CoreError> {
        self.write(|tx, _| tx.web_session_insert(&token_sha256, expires))
    }

    async fn web_session_delete(&self, token_sha256: [u8; 32]) -> Result<(), CoreError> {
        self.write(|tx, _| tx.web_session_delete(&token_sha256))
    }

    async fn api_keys(&self) -> Result<Vec<ApiKey>, CoreError> {
        self.store
            .read(auth::api_keys_list)
            .map(|keys| keys.into_iter().map(key_info).collect())
            .map_err(core_err)
    }

    async fn api_key_create(&self, key: ApiKey) -> Result<(), CoreError> {
        self.write(|tx, now| {
            tx.api_key_insert(&key.name, &key.key, now)?;
            if key.github_token.is_some()
                || key.github_login.is_some()
                || key.git_name.is_some()
                || key.git_email.is_some()
            {
                tx.api_key_update_github(
                    &key.name,
                    key.github_token.as_deref(),
                    key.github_login.as_deref(),
                    key.git_name.as_deref(),
                    key.git_email.as_deref(),
                )?;
            }
            Ok(())
        })
    }

    async fn api_key_delete(&self, name: &str) -> Result<(), CoreError> {
        self.write(|tx, _| tx.api_key_delete(name))
    }

    async fn api_key_set_github(
        &self,
        name: &str,
        github: Option<GithubAccount>,
    ) -> Result<(), CoreError> {
        self.write(|tx, _| {
            let g = github.as_ref();
            tx.api_key_update_github(
                name,
                g.map(|g| g.token.as_str()),
                g.map(|g| g.login.as_str()),
                g.map(|g| g.name.as_str()),
                g.map(|g| g.email.as_str()),
            )
            .map(drop)
        })
    }

    async fn meta(&self) -> Result<Value, CoreError> {
        let (issues, repos) = self
            .store
            .read(|c| Ok((issues::list_issues(c)?, femme_store::repos(c)?)))
            .map_err(core_err)?;
        let usage = *self.usage.lock().unwrap_or_else(|e| e.into_inner());
        Ok(scheduler::meta_json(
            &scheduler::meta_state(&issues, usage, &self.cfg),
            usage,
            &self.cfg,
            &repos,
            now_ms(),
        ))
    }

    async fn hook(&self, agent: Agent, payload: Value) -> Result<(), CoreError> {
        self.agent_hook(agent, payload)
    }

    fn config(&self) -> &Config {
        &self.cfg
    }
}

/// Once: the messages sent mid-turn that the herdr reader dropped before it knew them, put back
/// in their sessions ([`crate::headless::herdr::recover`]). A failure is logged, not fatal: the
/// daemon starts anyway and tries again next start.
fn recover_messages(store: &mut Store, cfg: &Config) {
    let done = cfg.state_path("recovered-queued-messages");
    if done.exists() {
        return;
    }
    let r = crate::headless::herdr::recover(store, &cfg.home, &cfg.state_path("uploads"));
    match r {
        Ok(n) => {
            if n > 0 {
                if let Err(e) = store.rotate_epoch() {
                    tracing::warn!(%e, "epoch not rotated after putting messages back");
                }
            }
            tracing::info!(n, "messages sent mid-turn: put back");
            let _ = std::fs::write(&done, n.to_string());
        }
        Err(e) => tracing::warn!(%e, "messages sent mid-turn: not put back"),
    }
}
