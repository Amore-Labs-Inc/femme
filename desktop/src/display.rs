//! Which display the main window opens on (`FEMME_DISPLAY`).
//!
//! gpui lists displays through `PlatformDisplay` and `WindowOptions::display_id` places the window on
//! one: macOS honours it, and so does X11 when it exposes several X screens. A Wayland
//! compositor places toplevel windows itself and gpui has no way to ask for an output, so there the
//! choice only fixes the window size and what the startup log says; pin the window with a compositor
//! rule instead (README, "Which display").
//!
//! Default with more than one display: the built-in one, never "the main display" (where the user
//! is usually working). `FEMME_DISPLAY` overrides: `builtin`, `primary` or an index into `cx.displays()`.

use gpui::{size, App, Bounds, DisplayId, Pixels, Size};

/// What `FEMME_DISPLAY` asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Want {
    /// The laptop's own panel.
    Builtin,
    /// The platform's main display (the first one where the platform has no such notion).
    Primary,
    /// An index into `cx.displays()`.
    Index(usize),
}

impl Want {
    /// Parses `builtin`, `primary` or a number; anything else is `None`.
    pub fn parse(s: &str) -> Option<Want> {
        match s.trim().to_ascii_lowercase().as_str() {
            "builtin" | "built-in" => Some(Want::Builtin),
            "primary" => Some(Want::Primary),
            n => n.parse().ok().map(Want::Index),
        }
    }
}

/// One display as [`choose`] sees it.
#[derive(Clone, Copy, Debug)]
pub struct Cand {
    /// The platform's main display.
    pub primary: bool,
    /// Known to be the built-in panel (only macOS can say).
    pub builtin: bool,
    /// Logical pixels, width times height.
    pub area: f32,
}

/// Index into `cands` (not empty) of the display to use. One display: that one. Otherwise `want`, and
/// without a usable `want` the built-in display: the one flagged built-in, else the smallest
/// non-primary one (a laptop panel is smaller than the monitor next to it).
pub fn choose(cands: &[Cand], want: Option<Want>) -> usize {
    if cands.len() < 2 {
        return 0;
    }
    match want {
        Some(Want::Index(i)) if i < cands.len() => i,
        Some(Want::Primary) => cands.iter().position(|c| c.primary).unwrap_or(0),
        _ => cands
            .iter()
            .position(|c| c.builtin)
            .or_else(|| {
                (0..cands.len())
                    .filter(|&i| !cands[i].primary)
                    .min_by(|&a, &b| cands[a].area.total_cmp(&cands[b].area))
            })
            .unwrap_or(0),
    }
}

/// What to say about a `FEMME_DISPLAY` value that cannot be honoured: not `builtin`, `primary` or a
/// number, or an index past the last of `displays`.
fn complaint(env: &str, displays: usize) -> Option<String> {
    match Want::parse(env) {
        None => Some(format!(
            "ignoring FEMME_DISPLAY={env} (use builtin, primary or an index)"
        )),
        Some(Want::Index(i)) if i >= displays => Some(format!(
            "ignoring FEMME_DISPLAY={env}: gpui lists {displays} display(s), counted from 0"
        )),
        Some(_) => None,
    }
}

/// Where the window goes.
pub struct Placement {
    /// For `WindowOptions::display_id`; `None` when gpui lists no displays.
    pub display_id: Option<DisplayId>,
    /// The window rectangle: centred on that display, at most 90% of it.
    pub bounds: Bounds<Pixels>,
}

/// Picks the display (see the module docs), logs the choice to stderr and returns the placement for a
/// window of `want` size.
pub fn place(cx: &App, want: Size<Pixels>) -> Placement {
    let displays = cx.displays();
    let env = std::env::var("FEMME_DISPLAY").ok();
    let wanted = env.as_deref().and_then(Want::parse);
    if let Some(msg) = env.as_deref().and_then(|e| complaint(e, displays.len())) {
        eprintln!("femme-desktop: {msg}");
    }
    if displays.is_empty() {
        eprintln!("femme-desktop: gpui lists no displays, the platform places the window");
        return Placement {
            display_id: None,
            bounds: Bounds::centered(None, want, cx),
        };
    }
    let primary = cx.primary_display().map(|d| d.id());
    let cands: Vec<Cand> = displays
        .iter()
        .map(|d| {
            let s = d.bounds().size;
            Cand {
                primary: Some(d.id()) == primary,
                builtin: is_builtin(d.id()),
                area: f32::from(s.width) * f32::from(s.height),
            }
        })
        .collect();
    let i = choose(&cands, wanted);
    let d = &displays[i];
    let b = d.bounds();
    let hz = d.refresh_interval().map_or_else(
        || "?".to_string(),
        |r| format!("{:.0}", 1.0 / r.as_secs_f64()),
    );
    eprintln!(
        "femme-desktop: display {i} of {}: {:.0}x{:.0} at {hz} Hz{}{} (FEMME_DISPLAY={}). {}",
        displays.len(),
        f32::from(b.size.width),
        f32::from(b.size.height),
        if cands[i].primary { ", primary" } else { "" },
        if cands[i].builtin { ", built-in" } else { "" },
        env.as_deref().unwrap_or("unset"),
        platform_note(displays.len()),
    );
    let fit = want.min(&size(b.size.width * 0.9, b.size.height * 0.9));
    Placement {
        display_id: Some(d.id()),
        bounds: Bounds::centered(Some(d.id()), fit, cx),
    }
}

fn platform_note(displays: usize) -> &'static str {
    if cfg!(target_os = "linux") && std::env::var_os("WAYLAND_DISPLAY").is_some() {
        "Wayland: the compositor decides which output shows the window; pin it with a compositor rule."
    } else if cfg!(target_os = "linux") && displays < 2 {
        "X11: one screen spans every monitor, so the window manager decides."
    } else {
        ""
    }
}

#[cfg(target_os = "macos")]
fn is_builtin(id: DisplayId) -> bool {
    // gpui's macOS `DisplayId` is the CGDirectDisplayID.
    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGDisplayIsBuiltin(display: u32) -> u32;
    }
    // ponytail: untested on macOS (not built there); this is one CoreGraphics call.
    unsafe { CGDisplayIsBuiltin(u64::from(id) as u32) != 0 }
}

#[cfg(not(target_os = "macos"))]
fn is_builtin(_: DisplayId) -> bool {
    // ponytail: only macOS can tell; elsewhere the smallest non-primary display stands in. Wayland output
    // names (eDP-*, LVDS-*) could be matched through `PlatformDisplay::uuid` (a v5 UUID of the name).
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(primary: bool, builtin: bool, w: f32, h: f32) -> Cand {
        Cand {
            primary,
            builtin,
            area: w * h,
        }
    }

    #[test]
    fn parses_the_env_values() {
        assert_eq!(Want::parse("builtin"), Some(Want::Builtin));
        assert_eq!(Want::parse(" Built-In "), Some(Want::Builtin));
        assert_eq!(Want::parse("PRIMARY"), Some(Want::Primary));
        assert_eq!(Want::parse("1"), Some(Want::Index(1)));
        assert_eq!(Want::parse(""), None);
        assert_eq!(Want::parse("left"), None);
        assert_eq!(Want::parse("-1"), None);
    }

    #[test]
    fn one_display_is_always_the_one() {
        for want in [
            None,
            Some(Want::Builtin),
            Some(Want::Primary),
            Some(Want::Index(3)),
        ] {
            assert_eq!(choose(&[c(true, false, 1920., 1080.)], want), 0);
        }
    }

    #[test]
    fn default_avoids_the_primary_display() {
        // The monitor is the main display and the bigger one; the laptop panel is the other.
        let d = [c(true, false, 2560., 1440.), c(false, false, 1920., 1200.)];
        assert_eq!(choose(&d, None), 1);
        assert_eq!(choose(&d, Some(Want::Builtin)), 1);
        assert_eq!(choose(&[d[1], d[0]], None), 0);
    }

    #[test]
    fn no_primary_info_falls_back_to_the_smallest() {
        // Wayland: `primary_display()` is None, so nothing is flagged primary.
        let d = [c(false, false, 2560., 1440.), c(false, false, 1920., 1200.)];
        assert_eq!(choose(&d, None), 1);
        assert_eq!(choose(&[d[1], d[0]], None), 0);
    }

    #[test]
    fn a_builtin_flag_beats_size() {
        // A 4K panel scaled down can be larger than the monitor beside it.
        let d = [c(true, false, 1920., 1080.), c(false, true, 2880., 1800.)];
        assert_eq!(choose(&d, None), 1);
        let d = [c(false, true, 2880., 1800.), c(true, false, 1920., 1080.)];
        assert_eq!(choose(&d, None), 0);
    }

    #[test]
    fn explicit_choices() {
        let d = [
            c(true, false, 2560., 1440.),
            c(false, true, 1920., 1200.),
            c(false, false, 1280., 720.),
        ];
        assert_eq!(choose(&d, Some(Want::Primary)), 0);
        assert_eq!(choose(&d, Some(Want::Index(2))), 2);
        assert_eq!(choose(&d, Some(Want::Index(0))), 0);
        // Out of range: the default (built-in).
        assert_eq!(choose(&d, Some(Want::Index(9))), 1);
        // No display is primary: `primary` is the first one.
        let d = [c(false, false, 1920., 1200.), c(false, false, 2560., 1440.)];
        assert_eq!(choose(&d, Some(Want::Primary)), 0);
    }

    #[test]
    fn out_of_range_display_is_reported() {
        assert_eq!(complaint("builtin", 2), None);
        assert_eq!(complaint("Primary", 1), None);
        assert_eq!(complaint("1", 2), None);
        let msg = complaint("2", 2).expect("index 2 of 2 displays is out of range");
        assert!(
            msg.contains("FEMME_DISPLAY=2") && msg.contains("2 display"),
            "{msg}"
        );
        assert!(complaint("0", 0).is_some());
        assert!(complaint("left", 2)
            .expect("unparsable")
            .contains("use builtin"));
    }
}
