//! The requests of the `ctrl` stream, each answered from the daemon's `Core` (or the store, for
//! reads the core has no call for). Errors keep the compat layer's meaning: `NotFound` is 404,
//! `Busy` and `Conflict` are 409, `Bad` is 400, `Internal` is 502.

use std::path::Path;

use femme_proto::rpc::{Err as WireErr, ErrCode, IssueField, Req, Resp, MAX_BLOB_PUT};
use femme_proto::state::{Agent, Mode};
use femme_proto::{OpId, Sid};
use femme_store::{items, sessions};

use super::{Ctx, BODY_MAX};
use crate::core::{Core, CoreError, CreateSession, PaneRef};
use crate::daemon::{core_err, Daemon};
use crate::session::MAIN;

/// The most rows a transcript page returns, whatever the client asked for.
const PAGE_MAX: usize = 500;

// What `BlobPut` may carry must be storable (and `BlobGet` readable) whole.
const _: () = assert!(MAX_BLOB_PUT <= femme_store::items::BLOB_BYTES_MAX);

pub(crate) fn err(code: ErrCode, msg: impl Into<String>) -> WireErr {
    WireErr {
        code,
        msg: msg.into(),
    }
}

pub(crate) fn wire(e: CoreError) -> WireErr {
    let code = match &e {
        CoreError::NotFound => ErrCode::NotFound,
        CoreError::Busy => ErrCode::Busy,
        CoreError::Conflict(_) => ErrCode::Conflict,
        CoreError::Bad(_) => ErrCode::Bad,
        CoreError::Internal(_) => ErrCode::Internal,
    };
    err(code, e.to_string())
}

/// Whether running `req` twice would do something twice. Only those are remembered per op id: a
/// replayed `Subscribe` must subscribe the *new* connection, and a page is not worth a row. A
/// `BlobPut` is not remembered either: the same bytes are the same blob, and a recorded hash could
/// outlive the blob (retention sweeps what no item refers to), where running it again stores it.
/// Nor is a `BoardApi` GET: it reads, and its answer (the accounts and their emails, up to 1 MiB
/// each time the desktop polls) would sit in the `ops` table for a day for nothing. A POST or
/// DELETE is, so a retry gets what the first try got (a new key's secret is shown once, and a
/// second `POST /api/keys` would only say the name is taken).
pub(crate) fn mutates(req: &Req) -> bool {
    if let Req::BoardApi { method, .. } = req {
        return !method.eq_ignore_ascii_case("GET");
    }
    !matches!(
        req,
        Req::Subscribe { .. }
            | Req::Unsubscribe { .. }
            | Req::TranscriptPage { .. }
            | Req::BlobGet { .. }
            | Req::BlobPut { .. }
            | Req::FsList { .. }
            | Req::FsRead { .. }
            | Req::Folders { make: false, .. }
            | Req::Caps
            | Req::ServerInfo
            | Req::Commands
            | Req::Prs
            | Req::PrVideo { .. }
            | Req::PrAutoFix { on: None }
    )
}

/// The most a request may weigh on the wire: [`BODY_MAX`], and for a `BlobPut` the blob's own cap
/// besides (its name and framing take the `BODY_MAX` part).
pub(crate) fn body_max(req: &Req) -> usize {
    match req {
        Req::BlobPut { .. } => MAX_BLOB_PUT + BODY_MAX,
        _ => BODY_MAX,
    }
}

fn done(r: Result<(), CoreError>) -> Result<Resp, WireErr> {
    r.map(|()| Resp::Ok).map_err(wire)
}

/// Runs `req`; `op` is its op id, which a prompt's stored message keeps so the desktop knows it.
pub(crate) async fn dispatch(ctx: &Ctx, op: OpId, req: Req) -> Result<Resp, WireErr> {
    let d = &ctx.daemon;
    match req {
        Req::Subscribe { scope, after } => d
            .hub
            .subscribe(ctx.client, scope, after)
            .await
            .map(|_| Resp::Ok)
            .map_err(|e| err(ErrCode::NotFound, e.to_string())),
        Req::Unsubscribe { scope } => {
            d.hub.unsubscribe(ctx.client, scope);
            Ok(Resp::Ok)
        }
        Req::TranscriptPage {
            sid,
            before_seq,
            limit,
        } => page(d, sid, before_seq, limit).await,
        Req::BlobGet { hash } => {
            let store = d.store.clone();
            match tokio::task::spawn_blocking(move || store.get_blob(&hash)).await {
                Ok(Ok(Some(bytes))) => Ok(Resp::Blob(bytes)),
                Ok(Ok(None)) => Err(wire(CoreError::NotFound)),
                Ok(Err(e)) => Err(wire(core_err(e))),
                Err(e) => Err(err(ErrCode::Internal, format!("blob read: {e}"))),
            }
        }
        Req::SessionCreate {
            cwd,
            agent,
            mode,
            label,
            name,
            prompt,
            model,
        } => create(ctx, cwd, agent, mode, label, name, prompt, model).await,
        Req::SessionClose { sid } => done(d.close_session(sid).await),
        Req::Prompt {
            sid,
            text,
            attachments,
            how,
        } => {
            let pane = main_pane(d, sid)?;
            done(
                d.prompt_attached(&pane, &text, &attachments, how, Some(op))
                    .await,
            )
        }
        Req::Interrupt { sid } => done(d.interrupt(sid).await),
        Req::Background { sid, tool_use_id } => {
            done(d.background(sid, tool_use_id.as_deref()).await)
        }
        Req::AnswerQuestion {
            sid,
            request_id,
            answer,
        } => done(d.answer(sid, &request_id, answer).await),
        Req::Takeover { sid, to } => done(d.takeover(sid, to).await),
        Req::SetModel { sid, model } => done(d.set_model(sid, &model).await),
        Req::PaneOpen { sid } => d.open_pane(sid).await.map(Resp::Pane).map_err(wire),
        Req::PaneClose { pane } => done(d.close_pane(pane).await),
        Req::IssueCreate { repo, title, body } => {
            if !valid_repo(&repo) {
                return Err(err(ErrCode::Bad, REPO_RULE));
            }
            let (title, body) = (title.trim(), body.trim());
            if title.is_empty() {
                return Err(err(ErrCode::Bad, "repo and title are required"));
            }
            d.issue_create(&repo, title, body)
                .await
                .map(|i| Resp::Issue(i.id))
                .map_err(wire)
        }
        Req::IssuePatch {
            id,
            base_rev: _,
            fields,
        } => {
            let fields = fields
                .into_iter()
                .map(issue_field)
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| err(ErrCode::Bad, e))?;
            // ponytail: every field is last-writer-wins and `base_rev` is not looked at: the
            // per-field 409 of `Tx::patch_issue` needs the revs the board edits against (M4).
            d.issue_patch(id, fields)
                .await
                .map(|i| Resp::Issue(i.id))
                .map_err(wire)
        }
        Req::IssueDelete { id } => done(d.issue_delete(id).await),
        Req::FsList { sid, path } => super::fs::list(d, sid, path).await,
        Req::FsRead { sid, path } => super::fs::read(d, sid, path).await,
        Req::Folders { path, make } => super::fs::folders(d, path, make).await,
        Req::Upload { .. } => Err(unsupported("uploads arrive with M5")),
        Req::BlobPut { data, .. } => blob_put(d, data).await,
        Req::ServerInfo => Ok(server_info(d).await),
        Req::CyclePermission { sid } => done(d.cycle_permission(sid).await),
        Req::SessionLabel { sid, label } => done(d.update(sid, |s| s.label = label).map(drop)),
        Req::SessionOf { agent_sid } => d
            .store
            .read(|c| femme_store::sessions::list_sessions(c, true))
            .map_err(|e| wire(core_err(e)))?
            .into_iter()
            .find(|s| s.agent_sid.as_deref() == Some(agent_sid.as_str()))
            .map(|s| Resp::Session(s.id))
            .ok_or_else(|| wire(CoreError::NotFound)),
        Req::Show {
            sid,
            name,
            blob,
            caption,
        } => done(d.show(sid, &name, blob, &caption).await),
        Req::Commands => Ok(Resp::Commands {
            names: crate::headless::commands(d),
        }),
        Req::BoardApi { method, path, body } => {
            super::board::call(&d.cfg, &method, &path, body).await
        }
        Req::Caps => super::caps::caps(&d.cfg).await,
        Req::SetCap {
            account,
            five_hour,
            weekly,
        } => super::caps::set_cap(&d.cfg, account, five_hour, weekly).await,
        Req::Prs => super::prs::list(d).await,
        Req::PrVideo { name } => super::prs::video(&d.cfg, &name).await,
        Req::PrMerge { number } => super::prs::merge(d, number).await,
        Req::PrFix { number } => super::prs::fix(d, owner(ctx), number).await,
        Req::Resend { sid, item_id } => done(d.resend(sid, &item_id).await),
        Req::PrAutoFix { on } => super::prs::set_auto_fix(&d.cfg, on).await,
        Req::PrClose { number } => super::prs::close(&d.cfg, number).await,
    }
}

/// Stores an upload as a blob and answers its hash. The request's `name` is not kept: a `Prompt`
/// names its attachments itself, and the blob is its bytes.
async fn blob_put(d: &Daemon, data: Vec<u8>) -> Result<Resp, WireErr> {
    if data.len() > MAX_BLOB_PUT {
        let why = format!("a blob is at most {MAX_BLOB_PUT} bytes");
        return Err(err(ErrCode::Bad, why));
    }
    let store = d.store.clone();
    match tokio::task::spawn_blocking(move || store.write(|tx| tx.put_blob_bytes(&data))).await {
        Ok(Ok(hash)) => Ok(Resp::BlobHash(hash)),
        Ok(Err(e)) => Err(wire(core_err(e))),
        Err(e) => Err(err(ErrCode::Internal, format!("blob write: {e}"))),
    }
}

/// What this daemon is: its version, and the commit `server/deploy.sh` left in `<state>/deployed`.
async fn server_info(d: &Daemon) -> Resp {
    let deployed = tokio::fs::read_to_string(d.cfg.state_path("deployed"))
        .await
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    Resp::ServerInfo {
        version: env!("CARGO_PKG_VERSION").to_string(),
        deployed,
    }
}

fn unsupported(msg: &str) -> WireErr {
    err(ErrCode::Unsupported, msg)
}

// What the board's API checks before anything reaches the store or the scheduler, checked here the
// same way: a repo goes onto a `git`/`gh` command line, a name is what `/api/agents/<name>` finds
// a session by, and the board trims what it keeps.
// ponytail: copies of `compat::util`'s `valid_repo` and `valid_name` and of the label bound in
// `compat::terminals`, which are private to that module. Make them `pub(crate)` there and call
// them from here, so the two cannot drift.
const REPO_RULE: &str = "repo must look like owner/name";
const NAME_RULE: &str = "name: a-z, 0-9 and -, up to 32 characters";
/// 9999-12-31: a `resume_at` past it is a typo, and `* 1000` must not overflow.
const MAX_UNIX_SECS: i64 = 253_402_300_799;
/// The longest label a session keeps, in characters.
const LABEL_MAX: usize = 60;

fn valid_repo(s: &str) -> bool {
    let ok = |p: &str| {
        !p.is_empty()
            && !p.starts_with(['.', '-'])
            && p.chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
    };
    matches!(s.split_once('/'), Some((o, n)) if ok(o) && ok(n))
}

fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 32
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !s.starts_with('-')
}

/// One field of a patch as the board takes it: strings trimmed, an empty note or PR link clearing
/// it, the repo checked, `resume_at` inside the calendar, and no session link (the daemon owns it:
/// the board's `PATCHABLE` has no `session`; it does have `resume_at`, whatever its comment says).
fn issue_field(f: IssueField) -> Result<IssueField, String> {
    let trimmed = |s: String| s.trim().to_string();
    let optional = |s: Option<String>| s.map(trimmed).filter(|s| !s.is_empty());
    Ok(match f {
        IssueField::Repo(s) if valid_repo(&s) => IssueField::Repo(s),
        IssueField::Repo(_) => return Err(REPO_RULE.into()),
        IssueField::Title(s) => IssueField::Title(trimmed(s)),
        IssueField::Body(s) => IssueField::Body(trimmed(s)),
        IssueField::Note(s) => IssueField::Note(optional(s)),
        IssueField::PrUrl(s) => IssueField::PrUrl(optional(s)),
        IssueField::Status(s) => IssueField::Status(s),
        IssueField::Session(_) => return Err("session: the daemon links sessions itself".into()),
        IssueField::ResumeAt(Some(ms)) if !(0..=MAX_UNIX_SECS * 1000).contains(&ms) => {
            return Err("resume_at must be a time between 1970 and the year 9999".into())
        }
        IssueField::ResumeAt(t) => IssueField::ResumeAt(t),
    })
}

/// The pane a prompt goes to: the session's main one, named the way the compat layer names it.
fn main_pane(d: &Daemon, sid: Sid) -> Result<PaneRef, WireErr> {
    let s = d.session(sid).map_err(wire)?;
    Ok(PaneRef {
        machine: s.name.is_some().then(|| "instinct".to_string()),
        handle: s.handle,
        no: MAIN,
    })
}

#[allow(clippy::too_many_arguments)] // the request's own fields, one each
/// Who a session a client asks for belongs to: its device, `device:ssh` over ssh.
fn owner(ctx: &Ctx) -> String {
    if ctx.device.is_nil() {
        "device:ssh".to_string()
    } else {
        format!("device:{}", ctx.device)
    }
}

async fn create(
    ctx: &Ctx,
    cwd: String,
    agent: Agent,
    mode: Mode,
    label: Option<String>,
    name: Option<String>,
    prompt: Option<String>,
    model: Option<String>,
) -> Result<Resp, WireErr> {
    let d = &ctx.daemon;
    // A name is for an agent: `/api/agents/<name>` finds the session by it and types into it.
    match &name {
        Some(_) if agent == Agent::Shell => {
            let why = "a shell has no name: names are for agents";
            return Err(err(ErrCode::Bad, why));
        }
        Some(n) if !valid_name(n) => return Err(err(ErrCode::Bad, NAME_RULE)),
        _ => {}
    }
    let label = label
        .map(|l| l.trim().chars().take(LABEL_MAX).collect::<String>())
        .filter(|l| !l.is_empty());
    // a desktop chat starts on Opus 5.5 (at medium effort, `spawn_process`)
    let model = match model {
        None if agent == Agent::Claude => Some("claude-opus-5-5".to_owned()),
        m => m,
    };
    let cwd = match cwd.trim() {
        "" => d.cfg.home.display().to_string(),
        c => c.to_string(),
    };
    // A relative folder would be one of the daemon's own working directory, wherever that is.
    if !Path::new(&cwd).is_absolute() {
        return Err(err(ErrCode::Bad, format!("{cwd}: not an absolute path")));
    }
    // ponytail: any existing folder, where the board's terminals stay under home: an enrolled
    // device is as trusted as an ssh login, and a shell session is a shell. Confine it here if
    // that ever stops being true.
    if !tokio::fs::metadata(&cwd).await.is_ok_and(|m| m.is_dir()) {
        return Err(err(ErrCode::Bad, format!("{cwd}: not a folder")));
    }
    let owner = owner(ctx);
    let info = d
        .create_session(CreateSession {
            cwd,
            agent,
            mode,
            label,
            name,
            owner,
            model: model.filter(|m| !m.trim().is_empty()),
        })
        .await
        .map_err(wire)?;
    if let Some(text) = prompt.filter(|t| !t.is_empty()) {
        let pane = PaneRef {
            machine: info.machine.clone(),
            handle: info.handle.clone(),
            no: MAIN,
        };
        if let Err(e) = d.prompt(&pane, &text).await {
            // The session exists (the desktop sees it in its subscription): say so, with the
            // reason. Always `Conflict`, whatever the reason was: it is remembered with the op, so
            // a retry is told the same instead of making a second session (`Busy` and `Internal`
            // are not remembered, and a prompt that goes wrong is often one of them).
            let why = format!(
                "session {} was created but its prompt was not delivered: {e}",
                info.sid
            );
            return Err(err(ErrCode::Conflict, why));
        }
    }
    Ok(Resp::Session(info.sid))
}

/// A page of a session's transcript, newest first. `before_seq` 0 (or `u64::MAX`) is "from the end".
async fn page(d: &Daemon, sid: Sid, before_seq: u64, limit: u16) -> Result<Resp, WireErr> {
    let limit = usize::from(limit).clamp(1, PAGE_MAX);
    let before = Some(before_seq).filter(|&b| b != 0);
    let store = d.store.clone();
    let read = tokio::task::spawn_blocking(move || {
        store.read(|c| {
            if sessions::get_session(c, sid)?.is_none() {
                return Err(femme_store::Error::NotFound);
            }
            // One more than asked for says whether another page exists.
            items::page_items(c, sid, before, limit + 1)
        })
    })
    .await;
    let mut items = match read {
        Ok(Ok(items)) => items,
        Ok(Err(e)) => return Err(wire(core_err(e))),
        Err(e) => return Err(err(ErrCode::Internal, format!("transcript read: {e}"))),
    };
    let more = items.len() > limit;
    items.truncate(limit);
    let next_before = items.last().map(|i| i.seq).filter(|_| more);
    Ok(Resp::Page { items, next_before })
}
