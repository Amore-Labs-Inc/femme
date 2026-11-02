//! The requests of the `ctrl` stream, each answered from the daemon's `Core` (or the store, for
//! reads the core has no call for). Errors keep the compat layer's meaning: `NotFound` is 404,
//! `Busy` and `Conflict` are 409, `Bad` is 400, `Internal` is 502.

use femme_proto::rpc::{Err as WireErr, ErrCode, Req, Resp};
use femme_proto::state::{Agent, Mode};
use femme_proto::Sid;
use femme_store::{items, sessions};

use super::Ctx;
use crate::core::{Core, CoreError, CreateSession, PaneRef};
use crate::daemon::{core_err, Daemon};
use crate::session::MAIN;

/// The most rows a transcript page returns, whatever the client asked for.
const PAGE_MAX: usize = 500;

pub(crate) fn err(code: ErrCode, msg: impl Into<String>) -> WireErr {
    WireErr {
        code,
        msg: msg.into(),
    }
}

fn wire(e: CoreError) -> WireErr {
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
/// replayed `Subscribe` must subscribe the *new* connection, and a page is not worth a row.
pub(crate) fn mutates(req: &Req) -> bool {
    !matches!(
        req,
        Req::Subscribe { .. }
            | Req::Unsubscribe { .. }
            | Req::TranscriptPage { .. }
            | Req::BlobGet { .. }
            | Req::FsList { .. }
            | Req::FsRead { .. }
    )
}

fn done(r: Result<(), CoreError>) -> Result<Resp, WireErr> {
    r.map(|()| Resp::Ok).map_err(wire)
}

pub(crate) async fn dispatch(ctx: &Ctx, req: Req) -> Result<Resp, WireErr> {
    let d = &ctx.daemon;
    match req {
        Req::Subscribe { scope, after } => d
            .hub
            .subscribe(ctx.client, scope, after)
            .await
            .map(|_| Resp::Ok)
            .map_err(|e| err(ErrCode::NotFound, e)),
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
        } => create(ctx, cwd, agent, mode, label, name, prompt).await,
        Req::SessionClose { sid } => done(d.close_session(sid).await),
        Req::Prompt {
            sid,
            text,
            attachments,
            ..
        } => {
            if !attachments.is_empty() {
                return Err(unsupported("attachments come with uploads (M5)"));
            }
            // ponytail: `how` is not looked at: every prompt is a Queue (the core's `prompt` goes
            // after the running turn). Steer and InterruptThenSend want the headless driver's inject
            // and an interrupt-first, which `Core` has no call for yet.
            let pane = main_pane(d, sid)?;
            done(d.prompt(&pane, &text).await)
        }
        Req::Interrupt { sid } => done(d.interrupt(sid).await),
        Req::AnswerQuestion {
            sid,
            request_id,
            answer,
        } => done(d.answer(sid, &request_id, answer).await),
        Req::Takeover { sid, to } => done(d.takeover(sid, to).await),
        Req::SetModel { sid, model } => done(d.set_model(sid, &model).await),
        Req::PaneOpen { sid } => d.open_pane(sid).await.map(Resp::Pane).map_err(wire),
        Req::PaneClose { pane } => done(d.close_pane(pane).await),
        Req::IssueCreate { repo, title, body } => d
            .issue_create(&repo, &title, &body)
            .await
            .map(|i| Resp::Issue(i.id))
            .map_err(wire),
        Req::IssuePatch {
            id,
            base_rev: _,
            fields,
        } => {
            // ponytail: every field is last-writer-wins and `base_rev` is not looked at: the
            // per-field 409 of `Tx::patch_issue` needs the revs the board edits against (M4).
            d.issue_patch(id, fields)
                .await
                .map(|i| Resp::Issue(i.id))
                .map_err(wire)
        }
        Req::IssueDelete { id } => done(d.issue_delete(id).await),
        Req::Upload { .. } | Req::FsList { .. } | Req::FsRead { .. } => {
            Err(unsupported("uploads and file access arrive with M5"))
        }
    }
}

fn unsupported(msg: &str) -> WireErr {
    err(ErrCode::Unsupported, msg)
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

async fn create(
    ctx: &Ctx,
    cwd: String,
    agent: Agent,
    mode: Mode,
    label: Option<String>,
    name: Option<String>,
    prompt: Option<String>,
) -> Result<Resp, WireErr> {
    let d = &ctx.daemon;
    let cwd = match cwd.trim() {
        "" => d.cfg.home.display().to_string(),
        c => c.to_string(),
    };
    // ponytail: any existing folder, where the board's terminals stay under home: an enrolled
    // device is as trusted as an ssh login, and a shell session is a shell. Confine it here if
    // that ever stops being true.
    if !tokio::fs::metadata(&cwd).await.is_ok_and(|m| m.is_dir()) {
        return Err(err(ErrCode::Bad, format!("{cwd}: not a folder")));
    }
    let owner = if ctx.device.is_nil() {
        "device:ssh".to_string()
    } else {
        format!("device:{}", ctx.device)
    };
    let info = d
        .create_session(CreateSession {
            cwd,
            agent,
            mode,
            label,
            name,
            owner,
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
            // The session exists (the desktop sees it in its subscription): say so, with the reason.
            let mut e = wire(e);
            e.msg = format!(
                "session {} was created but its prompt was not delivered: {}",
                info.sid, e.msg
            );
            return Err(e);
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
