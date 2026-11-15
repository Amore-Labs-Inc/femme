//! `/fork [prompt]`: a copy of a Claude chat's conversation as a chat of its own, in the same
//! folder, while the original goes on untouched. Claude Code's TUI has `/fork` (2.1.325), `claude
//! -p` does not, so FEMME makes the copy itself: a new session whose first process resumes the
//! original with `--fork-session` under an id of its own ([`femme_agents::Launch::fork_from`]).
//! The row menu's `fork` and `femmed open --fork` make the same session
//! ([`crate::session`]'s `open_session`); this is what a typed `/fork` adds to it.

use femme_proto::state::Session;
use femme_proto::{Id, Sid};

use super::{event_item, put, Put};
use crate::daemon::{now_ms, Daemon};

/// The first prompt of a `/fork` line's copy (empty: none).
pub(crate) fn prompt(text: &str) -> Option<&str> {
    let p = text.trim_start().strip_prefix("/fork")?;
    (p.is_empty() || p.starts_with(char::is_whitespace)).then(|| p.trim())
}

/// How long the original's title may be in a copy's label.
const TITLE_MAX: usize = 48;

/// The label of a copy of `parent` (`None`: no session holds the original, `cwd` names it):
/// `<title> · fork`, then `· fork 2`, ... while an open session in `open` has that one.
pub(crate) fn label(parent: Option<&Session>, cwd: &str, open: &[Session]) -> String {
    let filled = |o: &Option<String>| o.clone().filter(|t| !t.trim().is_empty());
    let title = parent
        .and_then(|p| filled(&p.label).or_else(|| filled(&p.name)))
        .or_else(|| {
            cwd.trim_end_matches('/')
                .rsplit('/')
                .next()
                .map(str::to_owned)
        })
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| "chat".to_owned());
    let title: String = title.chars().take(TITLE_MAX).collect();
    let taken = |l: &str| open.iter().any(|s| s.label.as_deref() == Some(l));
    let first = format!("{title} · fork");
    if !taken(&first) {
        return first;
    }
    (2..)
        .map(|n| format!("{first} {n}"))
        .find(|l| !taken(l))
        .unwrap_or(first)
}

/// The row in the original that says where its copy is. The original's agent never sees it.
pub(crate) fn note(d: &Daemon, sid: Sid, label: &str) {
    let text = format!("forked into {label}");
    let item = event_item(sid, format!("fork-{}", Id::new()), &text, now_ms());
    if let Err(e) = d.write_items(|tx, _, published| put(tx, sid, &Put::Done(item), published)) {
        tracing::warn!(%sid, %e, "fork note not stored");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_fork_and_its_prompt() {
        assert_eq!(prompt("/fork say hi"), Some("say hi"));
        assert_eq!(prompt(" /fork"), Some(""));
        assert_eq!(prompt("/forked hi"), None);
        assert_eq!(prompt("please /fork"), None);
    }

    #[test]
    fn a_copy_is_named_after_the_original_and_numbered_when_the_name_is_taken() {
        let row = |label: Option<&str>| Session {
            id: Id(1),
            handle: "h".into(),
            rev: 1,
            name: None,
            owner: "board".into(),
            agent: femme_proto::state::Agent::Claude,
            mode: femme_proto::state::Mode::Headless,
            cwd: "/srv/shop/".into(),
            label: label.map(str::to_owned),
            agent_sid: None,
            model: None,
            status: femme_proto::state::SessionStatus::Idle,
            attention: femme_proto::state::Attention::None,
            created: 0,
            closed: None,
            permission: None,
            context: None,
            effort: None,
            goal: None,
        };
        let parent = row(Some("Fix the cart"));
        assert_eq!(label(Some(&parent), "/x", &[]), "Fix the cart · fork");
        let open = [
            row(Some("Fix the cart · fork")),
            row(Some("Fix the cart · fork 2")),
        ];
        assert_eq!(label(Some(&parent), "/x", &open), "Fix the cart · fork 3");
        // no title, or no session at all: the folder names it, as in the sidebar
        assert_eq!(label(Some(&row(None)), "/srv/shop/", &[]), "shop · fork");
        assert_eq!(label(None, "/srv/shop", &[]), "shop · fork");
        let long = row(Some(&"x".repeat(80)));
        assert_eq!(label(Some(&long), "/x", &[]).chars().count(), TITLE_MAX + 7);
    }
}
