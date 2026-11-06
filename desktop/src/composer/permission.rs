//! The permission-mode square above the composer's `+`: a Claude session's permission mode as a
//! colour (green manual, yellow accept edits, purple plan, red bypass) with its name in the
//! tooltip. Pure, so it is tested without a window.

use gpui::{Rgba, SharedString};

use crate::theme::{accent_deep, bad, done, neon, panel, pause, work};

/// How the square shows a mode: a short label, its colour and the full name for the tooltip.
#[derive(Clone, Debug, PartialEq)]
pub struct Look {
    /// The mode's short name.
    pub label: SharedString,
    /// The square's fill.
    pub color: Rgba,
    /// What the tooltip says.
    pub tip: SharedString,
}

/// The look of `mode` as `femme_proto::state::Session::permission` has it. `default` is Claude's
/// own word for manual; a mode this build does not know is shown as it came, in panel grey.
pub fn look(mode: &str) -> Look {
    let (label, color, tip) = match mode {
        "manual" | "default" => ("manual", work(), "Manual (ask before tools)"),
        "acceptEdits" => ("accept edits", pause(), "Accept edits"),
        "plan" => ("plan", accent_deep(), "Plan mode"),
        "auto" => ("auto", neon(), "Auto mode"),
        "bypassPermissions" => ("bypass", bad(), "Bypass permissions"),
        "dontAsk" => ("don't ask", done(), "Don't ask"),
        other => {
            let other = SharedString::from(other.to_owned());
            return Look {
                label: other.clone(),
                color: panel(),
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
            ("manual", "manual", work(), "Manual (ask before tools)"),
            ("acceptEdits", "accept edits", pause(), "Accept edits"),
            ("plan", "plan", accent_deep(), "Plan mode"),
            ("auto", "auto", neon(), "Auto mode"),
            ("bypassPermissions", "bypass", bad(), "Bypass permissions"),
            ("dontAsk", "don't ask", done(), "Don't ask"),
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
        assert_eq!(l.color, panel());
    }
}
