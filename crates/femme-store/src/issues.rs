//! Issues: the board's queue, with per-field merge instead of last-writer-wins.
//!
//! Every issue has a row `rev` (its latest change) and `field_revs`, a map from field name to the
//! rev at which that field last changed. A client edits an issue with the `rev` it last saw as
//! `base_rev` ([`Tx::patch_issue`]); the edit is refused only if a field *it changes* has moved
//! past `base_rev`. Edits to different fields merge, so two devices typing into the title and the
//! body never lose each other's work, while two edits of the same field surface as
//! [`Error::Conflict`] for the user to resolve. The scheduler's own transitions
//! ([`Tx::set_issue_status`], [`Tx::update_issue`]) never conflict; they move the rev of every
//! field they change, which is what makes a stale human edit of the same field conflict.
//!
//! Field names (keys of `field_revs`): `repo`, `title`, `body`, `status`, `pr_url`, `note`,
//! `session`, `resume_at`.
//!
//! Optional text (`note`, `pr_url`) is `None` when empty: the board used `""` for "none".
//! A change that leaves every field as it was writes nothing and keeps the rev.
//!
//! Issue ids come from a counter that never goes back, so the id of a deleted issue is not
//! reused (the board used `max(id) + 1`, which could).

use std::collections::BTreeMap;

use femme_proto::rpc::IssueField;
use femme_proto::state::{Entity, Issue, IssueStatus};
use femme_proto::Sid;
use rusqlite::{params, Connection, Row};

use crate::enums::*;
use crate::{all, blob_id, enum_col, id_blob, meta_get, non_empty, one, Error, Result, Tx};

macro_rules! issue_cols {
    () => {
        "id, rev, repo, title, body, status, pr_url, note, session_id, resume_at, created, updated"
    };
}
pub(crate) use issue_cols;

pub(crate) fn issue_from_row(r: &Row<'_>) -> Result<Issue> {
    Ok(Issue {
        id: r.get(0)?,
        rev: r.get(1)?,
        repo: r.get(2)?,
        title: r.get(3)?,
        body: r.get(4)?,
        status: enum_col(r, 5, issue_status_parse)?,
        pr_url: r.get(6)?,
        note: r.get(7)?,
        session: r.get::<_, Option<[u8; 16]>>(8)?.map(blob_id),
        resume_at: r.get(9)?,
        created: r.get(10)?,
        updated: r.get(11)?,
    })
}

/// An issue with its `field_revs`.
struct Stored {
    issue: Issue,
    field_revs: BTreeMap<String, u64>,
}

fn load(conn: &Connection, id: u64) -> Result<Option<Stored>> {
    one(
        conn,
        concat!(
            "SELECT ",
            issue_cols!(),
            ", field_revs FROM issues WHERE id = ?1"
        ),
        [id],
        |r| {
            Ok(Stored {
                issue: issue_from_row(r)?,
                field_revs: serde_json::from_str(&r.get::<_, String>(12)?)?,
            })
        },
    )
}

/// One issue.
pub fn get_issue(conn: &Connection, id: u64) -> Result<Option<Issue>> {
    one(
        conn,
        concat!("SELECT ", issue_cols!(), " FROM issues WHERE id = ?1"),
        [id],
        issue_from_row,
    )
}

/// Every issue, by id.
pub fn list_issues(conn: &Connection) -> Result<Vec<Issue>> {
    all(
        conn,
        concat!("SELECT ", issue_cols!(), " FROM issues ORDER BY id"),
        [],
        issue_from_row,
    )
}

/// The issue worked in session `sid` (the newest, if several point at it).
pub fn issue_by_session(conn: &Connection, sid: Sid) -> Result<Option<Issue>> {
    one(
        conn,
        concat!(
            "SELECT ",
            issue_cols!(),
            " FROM issues WHERE session_id = ?1 ORDER BY id DESC LIMIT 1"
        ),
        [id_blob(sid)],
        issue_from_row,
    )
}

/// The rev of the last change of each field of an issue (`{}` for a field never changed since
/// creation). `NotFound` if the issue does not exist.
pub fn issue_field_revs(conn: &Connection, id: u64) -> Result<BTreeMap<String, u64>> {
    Ok(load(conn, id)?.ok_or(Error::NotFound)?.field_revs)
}

// An exhaustive match: a variant added to `IssueField` does not compile here until it is applied.
fn apply_field(issue: &mut Issue, f: &IssueField) {
    match f {
        IssueField::Repo(v) => issue.repo.clone_from(v),
        IssueField::Title(v) => issue.title.clone_from(v),
        IssueField::Body(v) => issue.body.clone_from(v),
        IssueField::Status(v) => issue.status = *v,
        IssueField::Note(v) => issue.note = non_empty(v.clone()),
        IssueField::PrUrl(v) => issue.pr_url = non_empty(v.clone()),
        IssueField::Session(v) => issue.session = *v,
        IssueField::ResumeAt(v) => issue.resume_at = *v,
    }
}

/// The fields (by `field_revs` name) in which two versions of an issue differ.
fn changed_fields(a: &Issue, b: &Issue) -> Vec<&'static str> {
    [
        ("repo", a.repo != b.repo),
        ("title", a.title != b.title),
        ("body", a.body != b.body),
        ("status", a.status != b.status),
        ("pr_url", a.pr_url != b.pr_url),
        ("note", a.note != b.note),
        ("session", a.session != b.session),
        ("resume_at", a.resume_at != b.resume_at),
    ]
    .into_iter()
    .filter_map(|(name, changed)| changed.then_some(name))
    .collect()
}

impl Tx<'_> {
    /// Creates an issue: status `Open`, no note/PR/session, `created = updated = now`. The id is
    /// one past the highest ever used.
    pub fn create_issue(&mut self, repo: &str, title: &str, body: &str, now: i64) -> Result<Issue> {
        let issue = Issue {
            id: self.next_issue_id()?,
            rev: 0,
            repo: repo.into(),
            title: title.into(),
            body: body.into(),
            status: IssueStatus::Open,
            pr_url: None,
            note: None,
            session: None,
            resume_at: None,
            created: now,
            updated: now,
        };
        self.insert_issue(&issue)
    }

    /// Inserts an issue *with its own id* and timestamps (the JSON import). `issue.rev` is
    /// ignored; the id counter moves past `issue.id`. `Sqlite` error if the id is taken.
    pub(crate) fn insert_issue(&mut self, issue: &Issue) -> Result<Issue> {
        let mut issue = issue.clone();
        issue.rev = self.next_rev();
        self.prepare_cached(concat!(
            "INSERT INTO issues (",
            issue_cols!(),
            ", field_revs) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, '{}')"
        ))?
        .execute(params![
            issue.id,
            issue.rev,
            issue.repo,
            issue.title,
            issue.body,
            issue_status_str(issue.status),
            non_empty(issue.pr_url.clone()),
            non_empty(issue.note.clone()),
            issue.session.map(id_blob),
            issue.resume_at,
            issue.created,
            issue.updated,
        ])?;
        let seq = self.issue_seq()?;
        if issue.id > seq {
            self.meta_set("issue_seq", &issue.id.to_string())?;
        }
        Ok(issue)
    }

    fn issue_seq(&self) -> Result<u64> {
        Ok(meta_get(self, "issue_seq")?
            .and_then(|v| v.parse().ok())
            .unwrap_or(0))
    }

    fn next_issue_id(&mut self) -> Result<u64> {
        let highest: u64 =
            self.query_row("SELECT COALESCE(MAX(id), 0) FROM issues", [], |r| r.get(0))?;
        Ok(self.issue_seq()?.max(highest) + 1)
    }

    /// Per-field merge edit; see the [module docs](crate::issues). `base_rev` is the rev of the
    /// issue as the caller last saw it. Applies `fields` in order (a field named twice: the last
    /// wins); a field whose value ends up unchanged is not a change and cannot conflict.
    /// `Err(Conflict)` names the conflicting fields and nothing is written; otherwise the row gets
    /// a fresh `rev`, each changed field's `field_revs` entry moves to it, and `updated = now`.
    pub fn patch_issue(
        &mut self,
        id: u64,
        base_rev: u64,
        fields: &[IssueField],
        now: i64,
    ) -> Result<Issue> {
        let Stored {
            issue: current,
            field_revs,
        } = load(self, id)?.ok_or(Error::NotFound)?;
        let mut next = current.clone();
        for f in fields {
            apply_field(&mut next, f);
        }
        let changed = changed_fields(&current, &next);
        let conflicts: BTreeMap<String, u64> = changed
            .iter()
            .filter_map(|&name| {
                let at = field_revs.get(name).copied().unwrap_or(0);
                (at > base_rev).then(|| (name.to_string(), at))
            })
            .collect();
        if !conflicts.is_empty() {
            return Err(Error::Conflict {
                field_revs: conflicts,
            });
        }
        self.save_issue(current, next, field_revs, &changed, now)
    }

    /// The scheduler's status change (never conflicts): sets `status` and `note` (`None` clears
    /// it, as the worker's `note working ""` did). Returns the issue; no write if both are
    /// already as asked.
    pub fn set_issue_status(
        &mut self,
        id: u64,
        status: IssueStatus,
        note: Option<String>,
        now: i64,
    ) -> Result<Issue> {
        self.update_issue(id, now, |i| {
            i.status = status;
            i.note = note;
        })
    }

    /// Lets `f` change an issue without any conflict check: the scheduler's way to set `status`,
    /// `note`, `pr_url`, `session` and `resume_at` together (start = `Working` + session; PR
    /// opened = `Pr` + `pr_url`; ..). `id`, `created` and `rev` cannot be changed. Each field `f`
    /// really changed gets the new rev in `field_revs`; no change, no write.
    pub fn update_issue(&mut self, id: u64, now: i64, f: impl FnOnce(&mut Issue)) -> Result<Issue> {
        let Stored {
            issue: current,
            field_revs,
        } = load(self, id)?.ok_or(Error::NotFound)?;
        let mut next = current.clone();
        f(&mut next);
        next.id = current.id;
        next.created = current.created;
        next.note = non_empty(next.note);
        next.pr_url = non_empty(next.pr_url);
        let changed = changed_fields(&current, &next);
        self.save_issue(current, next, field_revs, &changed, now)
    }

    fn save_issue(
        &mut self,
        current: Issue,
        mut next: Issue,
        mut field_revs: BTreeMap<String, u64>,
        changed: &[&'static str],
        now: i64,
    ) -> Result<Issue> {
        if changed.is_empty() {
            return Ok(current);
        }
        let rev = self.next_rev();
        for name in changed {
            field_revs.insert((*name).to_string(), rev);
        }
        next.rev = rev;
        next.updated = now;
        self.prepare_cached(
            "UPDATE issues SET rev = ?2, repo = ?3, title = ?4, body = ?5, status = ?6, pr_url = ?7, note = ?8,
                    session_id = ?9, resume_at = ?10, field_revs = ?11, updated = ?12
             WHERE id = ?1",
        )?
        .execute(params![
            next.id,
            rev,
            next.repo,
            next.title,
            next.body,
            issue_status_str(next.status),
            next.pr_url,
            next.note,
            next.session.map(id_blob),
            next.resume_at,
            serde_json::to_string(&field_revs)?,
            now,
        ])?;
        Ok(next)
    }

    /// Deletes an issue and leaves a tombstone. `NotFound` if it does not exist.
    pub fn delete_issue(&mut self, id: u64) -> Result<()> {
        let n = self.execute("DELETE FROM issues WHERE id = ?1", [id])?;
        if n == 0 {
            return Err(Error::NotFound);
        }
        self.tombstone(Entity::Issue, &id.to_string())?;
        Ok(())
    }
}
