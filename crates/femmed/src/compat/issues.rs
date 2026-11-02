//! `/api/issues`: the board's issues over the store's per-field patch. Times go out as unix
//! seconds, as the board always sent them.

use std::collections::HashMap;
use std::sync::Arc;

use axum::response::Response;
use femme_proto::rpc::IssueField;
use femme_proto::state::{Issue, IssueStatus};
use serde_json::{json, Value};

use super::util::{apply, status_str};
use super::{core_err, err, ok, App, Req};
use crate::core::Core;

/// An issue as the page reads it. `pane` is the main pane of the session working on it, which the
/// page's Chat button opens.
fn issue_json(i: &Issue, pane: Option<&String>) -> Value {
    let mut v = json!({
        "id": i.id, "repo": i.repo, "title": i.title, "body": i.body, "status": status_str(i.status),
        "created": i.created / 1000, "updated": i.updated / 1000,
    });
    let set = |v: &mut Value, k: &str, s: Option<&str>| {
        if let Some(s) = s.filter(|s| !s.is_empty()) {
            v[k] = s.into();
        }
    };
    set(&mut v, "pr_url", i.pr_url.as_deref());
    set(&mut v, "note", i.note.as_deref());
    set(&mut v, "pane", pane.map(String::as_str));
    if let Some(t) = i.resume_at {
        v["resume_at"] = (t / 1000).into();
    }
    v
}

/// The issues as JSON, with their panes: one look at the sessions, and only when an issue has one.
async fn with_panes(core: &dyn Core, issues: &[Issue]) -> Result<Vec<Value>, Response> {
    let mut panes = HashMap::new();
    if issues.iter().any(|i| i.session.is_some()) {
        match core.terminals().await {
            Ok(sessions) => panes.extend(
                sessions
                    .iter()
                    .filter_map(|s| Some((s.sid, s.main_pane_id()?))),
            ),
            // Not fatal: the phone UI throws on an error body and stops refreshing everything.
            Err(e) => tracing::warn!("issues listed without panes: {e}"),
        }
    }
    Ok(issues
        .iter()
        .map(|i| issue_json(i, i.session.and_then(|s| panes.get(&s))))
        .collect())
}

pub(super) async fn list(app: &Arc<App>) -> Response {
    let mut issues = match app.core.issues().await {
        Ok(i) => i,
        Err(e) => return core_err(e),
    };
    issues.sort_by_key(|i| i.id);
    match with_panes(app.core.as_ref(), &issues).await {
        Ok(v) => ok(Value::Array(v)),
        Err(r) => r,
    }
}

pub(super) async fn create(app: &Arc<App>, req: &mut Req) -> Response {
    let input = match req.json_or_400().await {
        Ok(v) => v,
        Err(r) => return r,
    };
    let fields = match apply(&input) {
        Ok(f) => f,
        Err(e) => return err(400, &e),
    };
    // The store makes `open` issues from repo, title and body; any other field a client sent goes on as a patch.
    let (mut repo, mut title, mut body, mut rest) =
        (String::new(), String::new(), String::new(), Vec::new());
    for f in fields {
        match f {
            IssueField::Repo(s) => repo = s,
            IssueField::Title(s) => title = s,
            IssueField::Body(s) => body = s,
            IssueField::Status(IssueStatus::Open) => {}
            other => rest.push(other),
        }
    }
    if repo.is_empty() || title.is_empty() {
        return err(400, "repo and title are required");
    }
    let mut issue = match app.core.issue_create(&repo, &title, &body).await {
        Ok(i) => i,
        Err(e) => return core_err(e),
    };
    if !rest.is_empty() {
        issue = match app.core.issue_patch(issue.id, rest).await {
            Ok(i) => i,
            Err(e) => return core_err(e),
        };
    }
    match with_panes(app.core.as_ref(), std::slice::from_ref(&issue)).await {
        Ok(v) => super::json_response(201, v.into_iter().next().unwrap_or_default()),
        Err(r) => r,
    }
}

/// The id as the board matched it: the number's own spelling, no padding or signs.
fn parse_id(id: &str) -> Option<u64> {
    id.parse::<u64>().ok().filter(|n| n.to_string() == id)
}

pub(super) async fn patch(app: &Arc<App>, req: &mut Req, id: &str) -> Response {
    let input = match req.json_or_400().await {
        Ok(v) => v,
        Err(r) => return r,
    };
    let Some(id) = parse_id(id) else {
        return err(404, "not found");
    };
    let fields = match apply(&input) {
        Ok(f) => f,
        // The board looked the issue up before reading the body: a bad field on a missing issue was a 404.
        Err(e) => {
            return match app.core.issues().await {
                Ok(all) if !all.iter().any(|i| i.id == id) => err(404, "not found"),
                _ => err(400, &e),
            }
        }
    };
    let issue = match app.core.issue_patch(id, fields).await {
        Ok(i) => i,
        Err(e) => return core_err(e),
    };
    match with_panes(app.core.as_ref(), std::slice::from_ref(&issue)).await {
        Ok(v) => ok(v.into_iter().next().unwrap_or_default()),
        Err(r) => r,
    }
}

pub(super) async fn delete(app: &Arc<App>, id: &str) -> Response {
    let Some(id) = parse_id(id) else {
        return err(404, "not found");
    };
    match app.core.issue_delete(id).await {
        Ok(()) => ok(json!({"ok": true})),
        Err(e) => core_err(e),
    }
}
