//! Low power: fewer and cheaper frames on a weak GPU (a Raspberry Pi 5 at 1440p).
//!
//! gpui redraws the whole window whenever anything calls `notify`, with no rate cap while the
//! window is active, so a streaming transcript or a busy terminal costs one full frame per
//! event, up to the display's refresh rate. [`Throttle`] holds those bursts to one frame per
//! [`budget`]: the first event of a quiet period draws at once, the rest wait for the budget
//! to pass and draw together. The switch ([`low`]) also turns motion off, draws the desk flat
//! and slows the tickers; it is on with `FEMME_LOW_POWER=1` (flag, environment or
//! `~/.config/femme/desktop.env`) or on a Raspberry Pi by itself.

use std::sync::OnceLock;
use std::time::Duration;

use gpui::{Context, Task};

static LOW: OnceLock<bool> = OnceLock::new();

/// Turns low power on or off for the process; the first call wins.
pub fn set(on: bool) {
    let _ = LOW.set(on);
}

/// Whether low power is on.
pub fn low() -> bool {
    LOW.get().copied().unwrap_or(false)
}

/// Whether `setting` (`FEMME_LOW_POWER`, `1`/`true`/`on`) or the machine asks for low power.
pub fn wanted(setting: Option<&str>, device_model: Option<&str>) -> bool {
    match setting.map(|s| s.trim().to_ascii_lowercase()) {
        Some(s) if ["1", "true", "on", "yes"].contains(&s.as_str()) => true,
        Some(s) if ["0", "false", "off", "no"].contains(&s.as_str()) => false,
        _ => device_model.is_some_and(|m| m.starts_with("Raspberry Pi")),
    }
}

/// `/proc/device-tree/model` (a Raspberry Pi names itself there), else `None`.
pub fn device_model() -> Option<String> {
    let raw = std::fs::read("/proc/device-tree/model").ok()?;
    Some(
        String::from_utf8_lossy(&raw)
            .trim_end_matches('\0')
            .to_owned(),
    )
}

/// The least time between two frames drawn for a burst of events: about 30 fps, 20 in low
/// power.
pub fn budget() -> Duration {
    Duration::from_millis(if low() { 50 } else { 33 })
}

/// How often a once-a-second readout (the "thinking… 42s" line) redraws.
pub fn tick() -> Duration {
    Duration::from_secs(if low() { 2 } else { 1 })
}

/// The decision of a [`Throttle`], pure: whether a hit draws now, and whether a budget's end
/// draws what came in meanwhile.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Gate {
    holding: bool,
    dirty: bool,
}

impl Gate {
    /// Something changed: `true` draws now (and starts a hold), `false` waits for the hold.
    pub fn hit(&mut self) -> bool {
        if self.holding {
            self.dirty = true;
            return false;
        }
        self.holding = true;
        true
    }

    /// The hold is over: `true` draws what came in during it (and starts the next hold).
    pub fn fire(&mut self) -> bool {
        self.holding = std::mem::take(&mut self.dirty);
        self.holding
    }
}

/// One frame per [`budget`] for a view that gets events in bursts.
#[derive(Default)]
pub struct Throttle {
    gate: Gate,
    hold: Option<Task<()>>,
}

impl Throttle {
    /// Something changed: draws now, or with the next frame of the budget. `get` finds this
    /// throttle in its view again when the budget ends.
    pub fn hit<T: 'static>(&mut self, get: fn(&mut T) -> &mut Throttle, cx: &mut Context<T>) {
        if self.gate.hit() {
            cx.notify();
            self.arm(get, cx);
        }
    }

    fn arm<T: 'static>(&mut self, get: fn(&mut T) -> &mut Throttle, cx: &mut Context<T>) {
        let wait = budget();
        self.hold = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(wait).await;
            this.update(cx, |t, cx| {
                let th = get(t);
                th.hold = None;
                if th.gate.fire() {
                    cx.notify();
                    th.arm(get, cx);
                }
            })
            .ok();
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_burst_draws_once_now_and_once_at_the_end() {
        let mut g = Gate::default();
        assert!(g.hit(), "the first event draws at once");
        assert!(!g.hit());
        assert!(!g.hit());
        assert!(
            g.fire(),
            "what came in meanwhile draws when the budget ends"
        );
        // the stream goes on: the next budget holds again
        assert!(!g.hit());
        assert!(g.fire());
        // quiet: the hold ends with nothing to draw, the next event draws at once
        assert!(!g.fire());
        assert!(g.hit());
    }

    #[test]
    fn the_switch_reads_the_setting_then_the_machine() {
        assert!(wanted(Some("1"), None));
        assert!(wanted(Some(" on "), Some("x")));
        assert!(!wanted(Some("0"), Some("Raspberry Pi 5 Model B Rev 1.1")));
        assert!(wanted(None, Some("Raspberry Pi 5 Model B Rev 1.1")));
        assert!(!wanted(None, Some("ThinkPad")));
        assert!(!wanted(None, None));
        assert!(!wanted(Some("maybe"), None));
    }
}
