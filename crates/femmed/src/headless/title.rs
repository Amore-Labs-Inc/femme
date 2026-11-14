//! A name for a chat: Claude Code titles a conversation by itself in its TUI (`ai-title` in the
//! transcript, read by [`super::backfill`]) but not under `-p`, so a headless chat with no label
//! yet is titled here after its first turn, by a fork of the conversation (`--resume
//! --fork-session`, nothing saved, no tools, the small model) asked for the title alone. A
//! label already there (the agents API's, a `/rename` read from disk) is kept.

use femme_proto::state::{Agent, Session};
use femme_proto::Sid;

use super::is_gateway;
use crate::daemon::Daemon;
use crate::session::{find_on_path, gateway_env};

/// The model that titles, for a Claude chat (a gateway chat keeps its own).
const SMALL: &str = "claude-haiku-5-5";
/// What the fork is asked.
const ASK: &str = "Give this conversation a title of three to six words, as the subject of what it is about. Answer with the title only: no quotes, no full stop, nothing else.";
/// A title longer than this is cut.
const MAX: usize = 60;

/// Whether `s` wants a title: a Claude chat with a conversation and no label yet.
pub(crate) fn wanted(s: &Session) -> bool {
    s.agent == Agent::Claude
        && s.agent_sid.is_some()
        && s.label.as_deref().is_none_or(|l| l.trim().is_empty())
}

/// Asks the fork for a title and puts it on the row when it comes, unless the row got a label
/// meanwhile. Errors are logged: a chat without a title is still a chat.
pub(crate) fn ask(d: &Daemon, s: &Session) {
    let Some(conv) = s.agent_sid.clone() else {
        return;
    };
    let path = d.sessions.path(&d.cfg);
    let Some(claude) = find_on_path("claude", &path) else {
        return;
    };
    let mut env = d.sessions.child_env(&d.cfg, &path);
    match gateway_env(&d.cfg.home, s.model.as_deref()) {
        Ok(e) => env.extend(e),
        Err(e) => {
            tracing::debug!(sid = %s.id, %e, "no title: no gateway env");
            return;
        }
    }
    let model = match s.model.as_deref() {
        Some(m) if is_gateway(Some(m)) => m.to_owned(),
        _ => SMALL.to_owned(),
    };
    let mut cmd = tokio::process::Command::new(claude);
    cmd.args([
        "-p",
        "--output-format=json",
        "--fork-session",
        "--no-session-persistence",
    ])
    .arg(format!("--resume={conv}"))
    .arg("--tools=")
    .arg(format!("--model={model}"))
    .arg(ASK)
    .current_dir(&s.cwd)
    .envs(env)
    .stdin(std::process::Stdio::null())
    .kill_on_drop(true);
    let (me, sid) = (d.me.clone(), s.id);
    tokio::spawn(async move {
        let out = tokio::time::timeout(std::time::Duration::from_secs(120), cmd.output()).await;
        let Some(d) = me.upgrade() else { return };
        let title = match &out {
            Ok(Ok(o)) => super::btw::answer_of(&o.stdout).and_then(|t| clean(&t)),
            Ok(Err(e)) => {
                tracing::debug!(%sid, %e, "no title");
                None
            }
            Err(_) => None,
        };
        let Some(title) = title else { return };
        put(&d, sid, title);
    });
}

/// The row's label, unless it got one meanwhile.
fn put(d: &Daemon, sid: Sid, title: String) {
    let r = d.update(sid, |s| {
        if s.label.as_deref().is_none_or(|l| l.trim().is_empty()) {
            s.label = Some(title.clone());
        }
    });
    if let Err(e) = r {
        tracing::debug!(%sid, %e, "title not stored");
    }
}

/// The title the fork answered, as a label: its first line, quotes and a full stop off, at most
/// [`MAX`] characters; `None` when nothing is left.
fn clean(answer: &str) -> Option<String> {
    let line = answer.lines().map(str::trim).find(|l| !l.is_empty())?;
    let t = line
        .trim_matches(|c: char| matches!(c, '"' | '\'' | '“' | '”' | '`' | '*'))
        .trim_end_matches('.')
        .trim();
    let t: String = t.chars().take(MAX).collect();
    Some(t.trim().to_owned()).filter(|t| !t.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_answer_becomes_a_label() {
        assert_eq!(
            clean("\"Fixing the login form.\"\n"),
            Some("Fixing the login form".into())
        );
        assert_eq!(clean("\n  **Minifig fork**  "), Some("Minifig fork".into()));
        assert_eq!(clean("\"\""), None);
        assert_eq!(clean(&"x".repeat(80)).unwrap().len(), MAX);
    }
}
