//! The permission-mode pill in the composer row: what a Claude session's permission mode is called
//! there, its colour and its tooltip. Pure, so it is tested without a window.

use gpui::{Rgba, SharedString};

use crate::theme::{BAD, DONE, MUTED, NEON, PANEL, PAUSE, PR};

/// How the pill shows a mode: a short label, a fill and the full name for the tooltip.
#[derive(Clone, Debug, PartialEq)]
pub struct Look {
    /// The pill's text.
    pub label: SharedString,
    /// The pill's fill (the label takes ink or paper, whichever reads).
    pub color: Rgba,
    /// What the tooltip says.
    pub tip: SharedString,
}

/// The look of `mode` as `femme_proto::state::Session::permission` has it. `default` is Claude's
/// own word for manual; a mode this build does not know is shown as it came, in panel grey.
pub fn look(mode: &str) -> Look {
    let (label, color, tip) = match mode {
        "manual" | "default" => ("manual", MUTED, "Manual (ask before tools)"),
        "acceptEdits" => ("accept edits", PAUSE, "Accept edits"),
        "plan" => ("plan", PR, "Plan mode"),
        "auto" => ("auto", NEON, "Auto mode"),
        "bypassPermissions" => ("bypass", BAD, "Bypass permissions"),
        "dontAsk" => ("don't ask", DONE, "Don't ask"),
        other => {
            let other = SharedString::from(other.to_owned());
            return Look {
                label: other.clone(),
                color: PANEL,
                tip: other,
            };
        }
    };
    Look {
        label: label.into(),
        color,
        tip: tip.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_mode_has_its_label_colour_and_name() {
        let cases = [
            ("manual", "manual", MUTED, "Manual (ask before tools)"),
            ("acceptEdits", "accept edits", PAUSE, "Accept edits"),
            ("plan", "plan", PR, "Plan mode"),
            ("auto", "auto", NEON, "Auto mode"),
            ("bypassPermissions", "bypass", BAD, "Bypass permissions"),
            ("dontAsk", "don't ask", DONE, "Don't ask"),
        ];
        for (mode, label, color, tip) in cases {
            let l = look(mode);
            assert_eq!(
                (l.label.as_ref(), l.color, l.tip.as_ref()),
                (label, color, tip),
                "{mode}"
            );
        }
    }

    #[test]
    fn default_is_manual() {
        assert_eq!(look("default"), look("manual"));
    }

    #[test]
    fn an_unknown_mode_is_shown_as_it_came() {
        let l = look("yolo");
        assert_eq!((l.label.as_ref(), l.tip.as_ref()), ("yolo", "yolo"));
        assert_eq!(l.color, PANEL);
    }
}
