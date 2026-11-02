//! One-time import of the old web board's JSON files, and the export that undoes it.
//!
//! The board (`board/src/main.rs`) keeps its state in `$FEMME_STATE` (default
//! `~/.local/state/femme`). All its times are unix **seconds**; the store's are milliseconds.
//!
//! | file | shape | imported as |
//! |---|---|---|
//! | `issues.json` | `[{id, repo, title, body, status, created, updated, pr_url?, note?, pane?, resume_at?}]` | issues, ids kept; `""` notes become none; `pane` (a herdr pane id) is dropped |
//! | `sessions.json` | `{token: expiry}` | web sessions keyed by `sha256(token)`; expired ones are skipped |
//! | `api-keys.json` | `[{name, key, created, github_token?, github_login?, git_name?, git_email?, github_error?}]` | API keys (`github_error` is dropped) |
//! | `meta.json` | `{state, usage, cap, repos, updated}` | only `repos` (see [`repos`]); the rest is scheduler output it rewrites every minute |
//!
//! Anything else in an issue or key is dropped and counted in [`ImportReport::dropped_fields`].
//! An issue whose `status` the store does not know (the old board once had `triage`, with a
//! `report` field) is imported as `failed` with the old status named in its note, never as `open`:
//! the scheduler would start an agent on it.
//!
//! # One shot
//!
//! [`Store::import_board_json`] parses and validates *every* file before it changes anything (a
//! file that is not what the board writes is a `Corrupt` error, nothing imported), copies the
//! originals to `<dir>/backups/<unix-seconds>/`, commits everything in one transaction, and only
//! then removes the originals. It sets `meta.json_imported`, and later calls return
//! `already_imported` without looking at the files, even when the first call found none: a board
//! file that reappears later must not overwrite live state. Stop the old board first.
//!
//! # Rollback
//!
//! [`Store::export_board_json`] writes the four files back in the board's format. Web sessions
//! cannot come back (only their hashes are stored), so `sessions.json` is `{}` and everyone signs
//! in again; issues lose their `session` link, which the old board never had.

use std::collections::{BTreeMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use femme_proto::state::{Issue, IssueStatus};
use serde_json::{json, Map, Value};

use crate::auth::{self, ApiKey};
use crate::enums::{issue_status_parse, issue_status_str};
use crate::{exists, issues, meta_get, non_empty, now_ms, repos, sha256, Error, Result, Store};

const FILES: [&str; 4] = ["issues.json", "sessions.json", "api-keys.json", "meta.json"];

/// What [`Store::import_board_json`] did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ImportReport {
    /// An import ran before; nothing was read or changed and every other field is empty.
    pub already_imported: bool,
    /// Issues imported.
    pub issues: usize,
    /// Web sessions imported.
    pub web_sessions: usize,
    /// Web sessions skipped because they had expired.
    pub web_sessions_expired: usize,
    /// API keys imported.
    pub api_keys: usize,
    /// Repos in the imported list.
    pub repos: usize,
    /// Fields that have no place in the new schema, with how many records had each
    /// (`pane`, `report`, `github_error`, ..).
    pub dropped_fields: BTreeMap<String, usize>,
    /// Things worth a look: duplicates skipped, unknown statuses, originals that could not be
    /// removed.
    pub warnings: Vec<String>,
    /// Where the original files were copied; `None` if there were none.
    pub backup_dir: Option<PathBuf>,
}

/// What [`Store::export_board_json`] wrote.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExportReport {
    /// Issues written.
    pub issues: usize,
    /// API keys written.
    pub api_keys: usize,
    /// Repos written.
    pub repos: usize,
    /// Live web sessions that could not be written (their tokens are not stored): those users
    /// sign in again.
    pub web_sessions_lost: usize,
}

#[derive(Default)]
struct Parsed {
    issues: Vec<Issue>,
    sessions: Vec<([u8; 32], i64)>,
    expired: usize,
    keys: Vec<ApiKey>,
    repos: Option<Vec<String>>,
    dropped: BTreeMap<String, usize>,
    warnings: Vec<String>,
}

impl Parsed {
    fn drop_rest(&mut self, o: &Map<String, Value>) {
        for k in o.keys() {
            *self.dropped.entry(k.clone()).or_default() += 1;
        }
    }
}

fn corrupt(what: impl std::fmt::Display) -> Error {
    Error::Corrupt(what.to_string())
}

/// Unix seconds as unix milliseconds.
fn ms(secs: u64) -> i64 {
    i64::try_from(secs).map_or(i64::MAX, |s| s.saturating_mul(1000))
}

fn read_file(dir: &Path, name: &str) -> Result<Option<Value>> {
    match fs::read_to_string(dir.join(name)) {
        Ok(text) => serde_json::from_str(&text)
            .map(Some)
            .map_err(|e| corrupt(format!("{name}: {e}"))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// A string field: `None` if absent or null; anything but a string is an error.
fn take_str(o: &mut Map<String, Value>, key: &str, at: &str) -> Result<Option<String>> {
    match o.remove(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(_) => Err(corrupt(format!("{at}: {key} is not a string"))),
    }
}

/// A unix-seconds field: `None` if absent or null.
fn take_secs(o: &mut Map<String, Value>, key: &str, at: &str) -> Result<Option<u64>> {
    match o.remove(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .map(Some)
            .ok_or_else(|| corrupt(format!("{at}: {key} is not unix seconds"))),
    }
}

fn parse_issues(v: Value, now: i64, out: &mut Parsed) -> Result<()> {
    let Value::Array(list) = v else {
        return Err(corrupt("issues.json: not an array"));
    };
    let mut seen = HashSet::new();
    for (n, raw) in list.into_iter().enumerate() {
        let Value::Object(mut o) = raw else {
            return Err(corrupt(format!("issues.json[{n}]: not an object")));
        };
        let id = o
            .remove("id")
            .and_then(|v| v.as_u64())
            .filter(|&id| id > 0 && id < i64::MAX as u64) // SQLite integers are i64, and the next id must fit too
            .ok_or_else(|| corrupt(format!("issues.json[{n}]: no usable id")))?;
        let at = format!("issues.json: issue {id}");
        if !seen.insert(id) {
            out.warnings
                .push(format!("{at}: appears twice, kept the first"));
            continue;
        }
        let repo =
            take_str(&mut o, "repo", &at)?.ok_or_else(|| corrupt(format!("{at}: no repo")))?;
        let title =
            take_str(&mut o, "title", &at)?.ok_or_else(|| corrupt(format!("{at}: no title")))?;
        let body = take_str(&mut o, "body", &at)?.unwrap_or_default();
        let status_text = take_str(&mut o, "status", &at)?.unwrap_or_else(|| "open".into());
        let mut note = non_empty(take_str(&mut o, "note", &at)?);
        let status = issue_status_parse(&status_text).unwrap_or_else(|| {
            let was = format!("was {status_text:?} on the old board");
            note = Some(note.take().map_or(was.clone(), |n| format!("{n} ({was})")));
            out.warnings.push(format!(
                "{at}: status {status_text:?} is not known, imported as failed"
            ));
            IssueStatus::Failed
        });
        let pr_url = non_empty(take_str(&mut o, "pr_url", &at)?);
        let created = take_secs(&mut o, "created", &at)?.map_or(now, ms);
        let updated = take_secs(&mut o, "updated", &at)?.map_or(created, ms);
        let resume_at = take_secs(&mut o, "resume_at", &at)?.map(ms);
        out.drop_rest(&o);
        out.issues.push(Issue {
            id,
            rev: 0,
            repo,
            title,
            body,
            status,
            pr_url,
            note,
            session: None,
            resume_at,
            created,
            updated,
        });
    }
    Ok(())
}

fn parse_sessions(v: Value, now: i64, out: &mut Parsed) -> Result<()> {
    let Value::Object(map) = v else {
        return Err(corrupt("sessions.json: not an object"));
    };
    for (token, expiry) in map {
        // never name the token in an error: it is a live credential
        let secs = expiry
            .as_u64()
            .ok_or_else(|| corrupt("sessions.json: an expiry is not unix seconds"))?;
        if ms(secs) <= now {
            out.expired += 1;
        } else {
            out.sessions.push((sha256(token.as_bytes()), ms(secs)));
        }
    }
    Ok(())
}

fn parse_keys(v: Value, now: i64, out: &mut Parsed) -> Result<()> {
    let Value::Array(list) = v else {
        return Err(corrupt("api-keys.json: not an array"));
    };
    for (n, raw) in list.into_iter().enumerate() {
        let at = format!("api-keys.json[{n}]");
        let Value::Object(mut o) = raw else {
            return Err(corrupt(format!("{at}: not an object")));
        };
        let name = take_str(&mut o, "name", &at)?
            .filter(|s| !s.is_empty())
            .ok_or_else(|| corrupt(format!("{at}: no name")))?;
        let at = format!("api-keys.json: key {name:?}");
        let key = take_str(&mut o, "key", &at)?
            .filter(|s| !s.is_empty())
            .ok_or_else(|| corrupt(format!("{at}: no key")))?;
        let created = take_secs(&mut o, "created", &at)?.map_or(now, ms);
        let github_token = non_empty(take_str(&mut o, "github_token", &at)?);
        let github_login = non_empty(take_str(&mut o, "github_login", &at)?);
        let git_name = non_empty(take_str(&mut o, "git_name", &at)?);
        let git_email = non_empty(take_str(&mut o, "git_email", &at)?);
        out.drop_rest(&o);
        out.keys.push(ApiKey {
            name,
            key,
            github_token,
            github_login,
            git_name,
            git_email,
            created,
        });
    }
    Ok(())
}

fn parse_meta(v: Value, out: &mut Parsed) -> Result<()> {
    let Value::Object(mut o) = v else {
        return Err(corrupt("meta.json: not an object"));
    };
    match o.remove("repos") {
        None | Some(Value::Null) => {}
        Some(Value::Array(list)) => {
            out.repos = Some(
                list.into_iter()
                    .filter_map(|r| r.as_str().map(str::to_owned))
                    .collect(),
            )
        }
        Some(_) => return Err(corrupt("meta.json: repos is not an array")),
    }
    Ok(())
}

/// Copies the files that exist to `<dir>/backups/<unix seconds>/` (0700; the copies keep the
/// originals' mode, which holds secrets).
fn backup(dir: &Path, present: &[&str], now: i64) -> Result<Option<PathBuf>> {
    if present.is_empty() {
        return Ok(None);
    }
    let target = dir.join("backups").join((now / 1000).to_string());
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&target)?;
    for name in present {
        fs::copy(dir.join(name), target.join(name))?;
    }
    Ok(Some(target))
}

impl Store {
    /// Imports the old board's files from `dir` (see the [module docs](self)); `now` is unix ms.
    /// Idempotent: after the first call it returns `already_imported`.
    pub fn import_board_json(&self, dir: &Path, now: i64) -> Result<ImportReport> {
        let done = ImportReport {
            already_imported: true,
            ..Default::default()
        };
        if self.read(|c| meta_get(c, "json_imported"))?.is_some() {
            return Ok(done);
        }

        let mut parsed = Parsed::default();
        let mut present = Vec::new();
        for name in FILES {
            let Some(v) = read_file(dir, name)? else {
                continue;
            };
            present.push(name);
            match name {
                "issues.json" => parse_issues(v, now, &mut parsed)?,
                "sessions.json" => parse_sessions(v, now, &mut parsed)?,
                "api-keys.json" => parse_keys(v, now, &mut parsed)?,
                _ => parse_meta(v, &mut parsed)?,
            }
        }
        let backup_dir = backup(dir, &present, now)?;

        let mut rep = ImportReport {
            web_sessions_expired: parsed.expired,
            dropped_fields: std::mem::take(&mut parsed.dropped),
            warnings: std::mem::take(&mut parsed.warnings),
            backup_dir,
            ..Default::default()
        };
        let committed = self.write_at(now, |tx| {
            if meta_get(tx, "json_imported")?.is_some() {
                return Ok(false); // another process imported while we were reading
            }
            for issue in &parsed.issues {
                if exists(tx, "SELECT 1 FROM issues WHERE id = ?1", [issue.id])? {
                    rep.warnings.push(format!(
                        "issue {} already exists in the database, kept that one",
                        issue.id
                    ));
                } else {
                    tx.insert_issue(issue)?;
                    rep.issues += 1;
                }
            }
            for (hash, expires) in &parsed.sessions {
                tx.web_session_insert(hash, *expires)?;
                rep.web_sessions += 1;
            }
            for key in &parsed.keys {
                if tx.api_key_import(key)? {
                    rep.api_keys += 1;
                } else {
                    rep.warnings.push(format!(
                        "api key {:?}: that name or key already exists, kept the existing one",
                        key.name
                    ));
                }
            }
            if let Some(list) = &parsed.repos {
                tx.set_repos(list)?;
                rep.repos = list.len();
            }
            tx.meta_set("json_imported", &now.to_string())?;
            Ok(true)
        })?;
        if !committed {
            return Ok(done);
        }
        // Committed: the originals are now only a stale copy. A failure here is not worth failing
        // for (the flag is set, they will not be read again), but the operator should know.
        for name in present {
            if let Err(e) = fs::remove_file(dir.join(name)) {
                rep.warnings
                    .push(format!("could not remove {name} after importing it: {e}"));
            }
        }
        Ok(rep)
    }

    /// Writes `issues.json`, `sessions.json`, `api-keys.json` and `meta.json` into `dir` in the
    /// old board's format, atomically (temp file + rename) and readable by the owner only, so
    /// `femme-board` can be started again. See the [module docs](self) for what is lost. The way
    /// back is one-way too: the import never runs twice on a database, so coming back to femmed
    /// after days on the board means a fresh database (move `femme.db` away first).
    pub fn export_board_json(&self, dir: &Path) -> Result<ExportReport> {
        let now = now_ms();
        let (issues, keys, repos, live) = self.read(|c| {
            let live: usize = c.query_row(
                "SELECT COUNT(*) FROM web_sessions WHERE expires > ?1",
                [now],
                |r| r.get(0),
            )?;
            Ok((
                issues::list_issues(c)?,
                auth::api_keys_list(c)?,
                repos(c)?,
                live,
            ))
        })?;

        let secs = |ms: i64| ms / 1000;
        let issue_json: Vec<Value> = issues
            .iter()
            .map(|i| {
                let mut o = json!({
                    "id": i.id, "repo": i.repo, "title": i.title, "body": i.body,
                    "status": issue_status_str(i.status),
                    "created": secs(i.created), "updated": secs(i.updated),
                });
                if let Some(url) = &i.pr_url {
                    o["pr_url"] = json!(url);
                }
                if let Some(note) = &i.note {
                    o["note"] = json!(note);
                }
                if let Some(at) = i.resume_at {
                    o["resume_at"] = json!(secs(at));
                }
                o
            })
            .collect();
        let key_json: Vec<Value> = keys
            .iter()
            .map(|k| {
                let mut o = json!({"name": k.name, "key": k.key, "created": secs(k.created)});
                for (field, value) in [
                    ("github_token", &k.github_token),
                    ("github_login", &k.github_login),
                    ("git_name", &k.git_name),
                    ("git_email", &k.git_email),
                ] {
                    if let Some(v) = value {
                        o[field] = json!(v);
                    }
                }
                o
            })
            .collect();
        // worker.sh rewrites meta.json within a minute; this is just a valid placeholder
        let meta = json!({"state": "exported from femmed", "usage": null, "cap": 60, "repos": repos, "updated": secs(now)});

        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
        write_private(
            &dir.join("issues.json"),
            &serde_json::to_string_pretty(&issue_json)?,
        )?;
        write_private(&dir.join("sessions.json"), "{}")?;
        write_private(
            &dir.join("api-keys.json"),
            &serde_json::to_string(&key_json)?,
        )?;
        write_private(&dir.join("meta.json"), &meta.to_string())?;
        Ok(ExportReport {
            issues: issue_json.len(),
            api_keys: key_json.len(),
            repos: repos.len(),
            web_sessions_lost: live,
        })
    }
}

/// Write-then-rename (a crash never leaves half a file), readable only by this user: how the
/// board saves its files.
///
/// The temp file is `<name>.femme-tmp`, not the board's `meta.tmp`, so a running `worker.sh`
/// cannot collide with it; a stale one is removed first and the new one made exclusively,
/// because the mode of `OpenOptions::mode` only applies to a file that is created, and these
/// files hold secrets.
fn write_private(path: &Path, data: &str) -> Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".femme-tmp");
    let tmp = PathBuf::from(tmp);
    match fs::remove_file(&tmp) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(data.as_bytes())?;
    f.sync_all()?;
    fs::rename(&tmp, path)?;
    Ok(())
}
