//! `/btw <question>`: a side question about a Claude session, answered without touching it. The
//! TUI has it, `claude -p` does not, so FEMME asks a fork of the conversation (`--resume
//! --fork-session`, nothing saved, no tools) and writes the question and the answer into the
//! session's transcript. The session's own turn, status and agent never see it.

use femme_agents::items::{body, AssistantMessage, UserMessage};
use femme_proto::state::{ItemKind, ItemStatus, Session};
use femme_proto::Id;
use serde_json::Value;

use super::{event_item, new_item, put, Put};
use crate::core::CoreError;
use crate::daemon::{now_ms, Daemon};
use crate::session::{find_on_path, gateway_env};

/// The question of a `/btw` prompt.
pub(crate) fn question(text: &str) -> Option<&str> {
    let q = text.trim_start().strip_prefix("/btw")?;
    (q.is_empty() || q.starts_with(char::is_whitespace)).then(|| q.trim())
}

/// Writes the question now and the answer when the fork has it.
pub(crate) fn ask(d: &Daemon, s: &Session, q: &str) -> Result<(), CoreError> {
    let Some(conv) = s.agent_sid.clone() else {
        return Err(CoreError::Bad(
            "/btw: the session has no conversation yet".into(),
        ));
    };
    if q.is_empty() {
        return Err(CoreError::Bad("/btw needs a question".into()));
    }
    let path = d.sessions.path(&d.cfg);
    let claude = find_on_path("claude", &path)
        .ok_or_else(|| CoreError::Internal("claude: not found on PATH".into()))?;
    let mut env = d.sessions.child_env(&d.cfg, &path);
    env.extend(gateway_env(&d.cfg.home, s.model.as_deref()).map_err(CoreError::Internal)?);
    let (sid, base) = (s.id, format!("btw-{}", Id::new()));
    let asked = new_item(
        sid,
        base.clone(),
        ItemKind::UserMessage,
        ItemStatus::Completed,
        Some(&body(&UserMessage {
            text: format!("/btw {q}"),
        })),
        now_ms(),
    );
    d.write_items(|tx, _, published| put(tx, sid, &Put::Done(asked), published))?;

    let mut cmd = tokio::process::Command::new(claude);
    cmd.args([
        "-p",
        "--output-format=json",
        "--fork-session",
        "--no-session-persistence",
    ])
    .arg(format!("--resume={conv}"))
    .arg("--tools="); // `--tools ""` would take the question for a tool too
    if let Some(m) = &s.model {
        cmd.arg(format!("--model={m}"));
    }
    cmd.arg(q)
        .current_dir(&s.cwd)
        .envs(env)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    let me = d.me.clone();
    tokio::spawn(async move {
        // a hung side question must not linger: the timeout drops the child, which kills it
        let out = tokio::time::timeout(std::time::Duration::from_secs(300), cmd.output()).await;
        let Some(d) = me.upgrade() else { return };
        let answer = match &out {
            Ok(Ok(o)) => answer_of(&o.stdout).ok_or_else(|| {
                let err = String::from_utf8_lossy(&o.stderr);
                format!("/btw failed: {}", err.trim())
            }),
            Ok(Err(e)) => Err(format!("/btw failed: {e}")),
            Err(_) => Err("/btw gave up after 5 minutes".into()),
        };
        let id = format!("{base}-a");
        let item = match answer {
            Ok(text) => new_item(
                sid,
                id,
                ItemKind::AssistantMessage,
                ItemStatus::Completed,
                Some(&body(&AssistantMessage { text })),
                now_ms(),
            ),
            Err(why) => event_item(sid, id, &why, now_ms()),
        };
        if let Err(e) = d.write_items(|tx, _, published| put(tx, sid, &Put::Done(item), published))
        {
            tracing::warn!(%sid, %e, "/btw answer not stored");
        }
    });
    Ok(())
}

/// The answer in `claude -p --output-format=json`'s result, unless it is an error.
fn answer_of(stdout: &[u8]) -> Option<String> {
    let v: Value = serde_json::from_slice(stdout).ok()?;
    if v["is_error"] == true {
        return None;
    }
    v["result"].as_str().map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_btw_and_its_question() {
        assert_eq!(question("/btw what is 2+2"), Some("what is 2+2"));
        assert_eq!(question("/btw"), Some(""));
        assert_eq!(question("/btwx hi"), None);
        assert_eq!(question("tell me /btw"), None);
        assert_eq!(
            answer_of(br#"{"is_error":false,"result":"4"}"#).as_deref(),
            Some("4")
        );
        assert_eq!(answer_of(br#"{"is_error":true,"result":"no"}"#), None);
    }
}
