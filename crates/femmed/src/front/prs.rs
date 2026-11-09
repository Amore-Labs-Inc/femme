//! `Prs`, `PrVideo`, `PrMerge` and `PrClose`: the box repo's open pull requests, through `gh` in
//! the box's checkout (`<home>/femme`). A merge lands on main and `server/deploy.sh` ships it.
//!
//! A pull request's demo video is `<state>/pr-videos/<branch, '/' as '-'>.<mp4|webm|gif>`, which
//! `server/claude-update.sh`'s implementer records.

use std::path::{Path, PathBuf};

use femme_proto::rpc::{Err as WireErr, ErrCode, MAX_BLOB_PUT, Pr, Resp};
use serde::Deserialize;
use tokio::process::Command;

use super::req::err;
use crate::config::Config;

const VIDEO_EXTS: [&str; 3] = ["mp4", "webm", "gif"];

/// `gh pr list --json`'s shape for the fields asked.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GhPr {
    number: u64,
    title: String,
    head_ref_name: String,
    author: GhAuthor,
    url: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    additions: u64,
    #[serde(default)]
    deletions: u64,
}

#[derive(Deserialize)]
struct GhAuthor {
    login: String,
}

/// Runs `gh` in the box's checkout and answers its stdout.
async fn gh(cfg: &Config, args: &[&str]) -> Result<Vec<u8>, WireErr> {
    let out = Command::new("gh")
        .args(args)
        .current_dir(cfg.home.join("femme"))
        .output()
        .await
        .map_err(|e| err(ErrCode::Internal, format!("gh: {e}")))?;
    if !out.status.success() {
        let why = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(err(
            ErrCode::Internal,
            format!("gh {}: {why}", args[..2].join(" ")),
        ));
    }
    Ok(out.stdout)
}

fn videos(cfg: &Config) -> PathBuf {
    cfg.state_path("pr-videos")
}

/// The demo video's file name for `branch`, when the folder has one.
fn video_for(dir: &Path, branch: &str) -> Option<String> {
    let stem = branch.replace('/', "-");
    VIDEO_EXTS
        .iter()
        .map(|ext| format!("{stem}.{ext}"))
        .find(|name| dir.join(name).is_file())
}

pub(crate) async fn list(cfg: &Config) -> Result<Resp, WireErr> {
    let fields = "number,title,headRefName,author,url,body,additions,deletions";
    let out = gh(
        cfg,
        &[
            "pr", "list", "--state", "open", "--limit", "100", "--json", fields,
        ],
    )
    .await?;
    let prs: Vec<GhPr> = serde_json::from_slice(&out)
        .map_err(|e| err(ErrCode::Internal, format!("gh pr list: {e}")))?;
    let dir = videos(cfg);
    Ok(Resp::Prs(
        prs.into_iter()
            .map(|p| Pr {
                video: video_for(&dir, &p.head_ref_name),
                number: p.number,
                title: p.title,
                branch: p.head_ref_name,
                author: p.author.login,
                url: p.url,
                body: p.body,
                additions: p.additions,
                deletions: p.deletions,
            })
            .collect(),
    ))
}

/// A name `video_for` could have given: one file in the folder, nothing that climbs out.
fn plain_name(name: &str) -> bool {
    !name.is_empty() && !name.starts_with('.') && !name.contains(['/', '\\'])
}

pub(crate) async fn video(cfg: &Config, name: &str) -> Result<Resp, WireErr> {
    if !plain_name(name) {
        return Err(err(ErrCode::Bad, "a video is a file name in pr-videos"));
    }
    let data = tokio::fs::read(videos(cfg).join(name))
        .await
        .map_err(|e| err(ErrCode::NotFound, format!("{name}: {e}")))?;
    if data.len() > MAX_BLOB_PUT {
        return Err(err(
            ErrCode::Bad,
            format!("{name} is over {MAX_BLOB_PUT} bytes"),
        ));
    }
    Ok(Resp::Blob(data))
}

pub(crate) async fn merge(cfg: &Config, number: u64) -> Result<Resp, WireErr> {
    let n = number.to_string();
    gh(cfg, &["pr", "merge", &n, "--squash", "--delete-branch"]).await?;
    Ok(Resp::Ok)
}

pub(crate) async fn close(cfg: &Config, number: u64) -> Result<Resp, WireErr> {
    let n = number.to_string();
    gh(cfg, &["pr", "close", &n, "--delete-branch"]).await?;
    Ok(Resp::Ok)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_branch_finds_its_video_by_its_dashed_name() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("claude-update-2.1.319-recap.mp4"), b"x").unwrap();
        assert_eq!(
            video_for(dir.path(), "claude-update/2.1.319/recap").as_deref(),
            Some("claude-update-2.1.319-recap.mp4")
        );
        assert_eq!(video_for(dir.path(), "other"), None);
    }

    #[test]
    fn a_video_name_cannot_leave_the_folder() {
        assert!(plain_name("a-b.mp4"));
        for bad in ["", "../x.mp4", "a/b.mp4", ".hidden", "a\\b"] {
            assert!(!plain_name(bad), "{bad}");
        }
    }
}
