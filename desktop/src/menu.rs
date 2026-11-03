//! The `femme` button at the top left and the dropdown it opens: version, check for updates,
//! accounts, reconnect, quit. Owned by the menu agent (docs/desktop-ui.md §menu); the shell mounts
//! it and handles [`MenuEvent::Accounts`], [`MenuEvent::Reconnect`] and [`MenuEvent::Quit`].
//!
//! "Check for updates" asks the daemon which commit the box runs and compares it with this build's
//! ([`COMMIT`], stamped by `build.rs`). The comparison is pure ([`judge`], [`compare`]) and tested;
//! the answer is a [`Verdict`] with the words to show under the item.

use std::time::Duration;

use femme_proto::rpc::Resp;
use gpui::{div, Context, Entity, EventEmitter, IntoElement, ParentElement, Render, Rgba, Window};

use crate::net::Net;
use crate::theme::{BAD, DONE, PAUSE, WORK};

/// This build's commit: a short sha, or `dev` when `build.rs` had neither `FEMME_COMMIT` nor git.
pub const COMMIT: &str = env!("FEMME_COMMIT");
/// This build's version, from `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// How long the pointer rests on the button before the menu opens (a fly-over does not open it).
pub const OPEN_AFTER: Duration = Duration::from_millis(120);
/// How long the pointer may be off both the button and the menu before a hover-opened menu closes.
pub const CLOSE_AFTER: Duration = Duration::from_millis(300);
/// How long the result of "check for updates" stays under its item.
pub const RESULT_FOR: Duration = Duration::from_secs(10);
/// What the menu asks the shell to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuEvent {
    /// Open the accounts overlay.
    Accounts,
    /// A check for updates started. The menu runs it and shows the answer itself: the shell has
    /// nothing to do.
    CheckUpdates,
    /// Drop the connection and connect again.
    Reconnect,
    /// Close the app.
    Quit,
}

/// What a check for updates found out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// The box runs the commit this build was made from.
    Current {
        /// This build's commit.
        commit: String,
    },
    /// The box runs another commit: the update timer on the PC installs it.
    Behind {
        /// The commit the box runs.
        deployed: String,
    },
    /// This build has no commit stamp, so there is nothing to compare with.
    Unstamped {
        /// The commit the box runs.
        deployed: String,
    },
    /// The box has no `deployed` marker.
    NoMarker,
    /// The daemon does not know `ServerInfo` yet.
    DaemonTooOld,
    /// The request failed, or there was nobody to ask.
    Failed(String),
}

/// `sha` cut to the seven characters that name a commit in a sentence.
pub fn short(sha: &str) -> &str {
    match sha.char_indices().nth(7) {
        Some((end, _)) => &sha[..end],
        None => sha,
    }
}

/// Whether `running` (this build's short sha) and `deployed` (the box's, usually the full one)
/// name the same commit: one is a prefix of the other.
fn same_commit(running: &str, deployed: &str) -> bool {
    let (a, b) = (running.to_ascii_lowercase(), deployed.to_ascii_lowercase());
    !a.is_empty() && !b.is_empty() && (b.starts_with(&a) || a.starts_with(&b))
}

/// Compares this build's commit with what the box says it runs. `running` is a short sha or
/// something that is not one (`dev`: a build made outside git).
pub fn compare(running: &str, deployed: Option<&str>) -> Verdict {
    let Some(deployed) = deployed.map(str::trim).filter(|d| !d.is_empty()) else {
        return Verdict::NoMarker;
    };
    let stamped = !running.is_empty() && running.bytes().all(|b| b.is_ascii_hexdigit());
    if !stamped {
        Verdict::Unstamped {
            deployed: deployed.to_owned(),
        }
    } else if same_commit(running, deployed) {
        Verdict::Current {
            commit: running.to_owned(),
        }
    } else {
        Verdict::Behind {
            deployed: deployed.to_owned(),
        }
    }
}

/// The verdict for the daemon's answer to `Req::ServerInfo`: `answer` is what [`Net::run`] gave,
/// `error` the reason it keeps in `Net::error` when the answer is `None`.
pub fn judge(running: &str, answer: Option<Resp>, error: Option<&str>) -> Verdict {
    match answer {
        Some(Resp::ServerInfo { deployed, .. }) => compare(running, deployed.as_deref()),
        Some(_) => Verdict::Failed("unexpected answer from the daemon".into()),
        // The client words a refused request as `<ErrCode>: <message>`.
        None => match error {
            Some(e) if e.starts_with("Unsupported") => Verdict::DaemonTooOld,
            Some(e) => Verdict::Failed(e.to_owned()),
            None => Verdict::Failed("no answer".into()),
        },
    }
}

impl Verdict {
    /// The words under the item.
    pub fn text(&self) -> String {
        match self {
            Verdict::Current { commit } => format!("up to date ({})", short(commit)),
            Verdict::Behind { deployed } => format!(
                "box runs {}: femme-desktop-update.timer installs it within 10 min, then restart",
                short(deployed)
            ),
            Verdict::Unstamped { deployed } => format!(
                "this build has no commit stamp; box runs {}",
                short(deployed)
            ),
            Verdict::NoMarker => "box has no deployed marker".into(),
            Verdict::DaemonTooOld => "daemon too old".into(),
            Verdict::Failed(why) => why.clone(),
        }
    }

    /// The colour of the marker in front of the words: green current, amber when something is
    /// waiting, grey when there is nothing to compare, red when it failed.
    pub fn tone(&self) -> Rgba {
        match self {
            Verdict::Current { .. } => WORK,
            Verdict::Behind { .. } | Verdict::DaemonTooOld => PAUSE,
            Verdict::Unstamped { .. } | Verdict::NoMarker => DONE,
            Verdict::Failed(_) => BAD,
        }
    }
}

/// The `femme` button and its dropdown.
pub struct FemmeMenu {
    #[allow(dead_code)]
    net: Option<Entity<Net>>,
}

impl FemmeMenu {
    /// A closed menu; `net` is the daemon's client, `None` on a synthetic run.
    pub fn new(net: Option<Entity<Net>>, _cx: &mut Context<Self>) -> Self {
        FemmeMenu { net }
    }
}

impl EventEmitter<MenuEvent> for FemmeMenu {}

impl Render for FemmeMenu {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().child("femme") // stub until the button and dropdown land
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: &str = "8b23d2d0f5c3a1e94b7d6a2c10e8f9b3a4d5c6e7";

    fn info(deployed: Option<&str>) -> Option<Resp> {
        Some(Resp::ServerInfo {
            version: "0.1.0".into(),
            deployed: deployed.map(str::to_owned),
        })
    }

    #[test]
    fn the_same_commit_is_up_to_date() {
        assert_eq!(
            compare("8b23d2d", Some("8b23d2d")),
            Verdict::Current {
                commit: "8b23d2d".into()
            }
        );
    }

    #[test]
    fn a_short_sha_matches_the_full_one_by_prefix() {
        let v = compare("8b23d2d", Some(FULL));
        assert_eq!(
            v,
            Verdict::Current {
                commit: "8b23d2d".into()
            }
        );
        assert_eq!(v.text(), "up to date (8b23d2d)");
        // The marker is a file: a trailing newline and capitals change nothing.
        assert!(matches!(
            compare("8B23D2D", Some(&format!("{FULL}\n"))),
            Verdict::Current { .. }
        ));
        // An eight-character stamp (git lengthens short shas when seven are ambiguous).
        assert!(matches!(
            compare("8b23d2d0", Some(FULL)),
            Verdict::Current { .. }
        ));
    }

    #[test]
    fn another_commit_says_what_the_timer_will_do() {
        let v = compare("8b23d2d", Some("1f2e3d4c5b6a79880011223344556677889900aa"));
        assert_eq!(
            v,
            Verdict::Behind {
                deployed: "1f2e3d4c5b6a79880011223344556677889900aa".into()
            }
        );
        assert_eq!(
            v.text(),
            "box runs 1f2e3d4: femme-desktop-update.timer installs it within 10 min, then restart"
        );
        // A different eighth character is a different commit: the prefix is not just seven long.
        assert!(matches!(
            compare("8b23d2d1", Some(FULL)),
            Verdict::Behind { .. }
        ));
    }

    #[test]
    fn a_box_without_a_marker_has_nothing_to_compare() {
        for deployed in [None, Some(""), Some("  \n")] {
            let v = compare("8b23d2d", deployed);
            assert_eq!(v, Verdict::NoMarker);
            assert_eq!(v.text(), "box has no deployed marker");
        }
    }

    #[test]
    fn a_build_without_a_stamp_is_not_called_behind() {
        let v = compare("dev", Some(FULL));
        assert_eq!(v.text(), "this build has no commit stamp; box runs 8b23d2d");
        assert!(matches!(compare("", Some(FULL)), Verdict::Unstamped { .. }));
        // No marker still wins: that is the more useful thing to say.
        assert_eq!(compare("dev", None), Verdict::NoMarker);
    }

    #[test]
    fn an_old_daemon_is_told_apart_from_a_failed_request() {
        let old = judge("8b23d2d", None, Some("Unsupported: unknown request"));
        assert_eq!(old, Verdict::DaemonTooOld);
        assert_eq!(old.text(), "daemon too old");
        let timeout = judge("8b23d2d", None, Some("no answer within the deadline"));
        assert_eq!(timeout.text(), "no answer within the deadline");
        assert_eq!(judge("8b23d2d", None, None).text(), "no answer");
    }

    #[test]
    fn the_answer_is_judged_by_its_marker() {
        assert!(matches!(
            judge("8b23d2d", info(Some(FULL)), None),
            Verdict::Current { .. }
        ));
        assert_eq!(judge("8b23d2d", info(None), None), Verdict::NoMarker);
        // A stale error from an earlier request does not spoil a good answer.
        assert!(matches!(
            judge("8b23d2d", info(Some(FULL)), Some("Unsupported: x")),
            Verdict::Current { .. }
        ));
        assert!(matches!(
            judge("8b23d2d", Some(Resp::Ok), None),
            Verdict::Failed(_)
        ));
    }

    #[test]
    fn short_cuts_at_seven_characters_and_never_inside_one() {
        assert_eq!(short(FULL), "8b23d2d");
        assert_eq!(short("abc"), "abc");
        assert_eq!(short("ééééééééé"), "ééééééé");
    }

    #[test]
    fn this_build_knows_its_commit() {
        assert!(!COMMIT.is_empty());
        assert!(!VERSION.is_empty());
    }
}
