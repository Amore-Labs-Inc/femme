//! How things appear and go: a short ease-out fade with a small move, in the Mac's manner (a
//! panel slides in from its edge, a menu drops down, a card rises). One curve for the whole app,
//! sampled every frame at the display's rate by gpui-base's `Presence`, which keeps an exiting
//! element on screen until its fade is over and stands still under the OS's "reduce motion".

use std::time::Duration;

use gpui::{px, App, ElementId, Pixels, Window};
use gpui_base::animation::cubic_bezier;
use gpui_base::{Presence, Transition};

/// Appear: 180 ms (T3 Code's drawer curve, `cubic-bezier(.32,.72,0,1)`, settles fast, no bounce).
const APPEAR: Duration = Duration::from_millis(180);

/// A slide: 160 ms, the same curve.
const SLIDE: Duration = Duration::from_millis(160);

/// `d` stretched by `FEMME_MOTION_SCALE` (a headful check sets 10 to watch a transition frame by
/// frame; unset, it is 1).
pub(crate) fn scaled(d: Duration) -> Duration {
    static SCALE: std::sync::OnceLock<f32> = std::sync::OnceLock::new();
    let scale = *SCALE.get_or_init(|| {
        std::env::var("FEMME_MOTION_SCALE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1.)
    });
    d.mul_f32(scale)
}

/// Where an element is in its appearance, this frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Appear {
    /// 0 (gone) to 1 (fully there).
    pub t: f32,
}

impl Appear {
    /// `1` when there, lower while fading either way.
    pub fn opacity(self) -> f32 {
        self.t
    }

    /// How far from its resting place the element is, out of `travel`: `travel` when gone, zero
    /// when there.
    pub fn offset(self, travel: f32) -> Pixels {
        px(travel * (1. - self.t))
    }
}

/// Sample `id`'s appearance this frame: `None` when it is gone and need not be drawn, `Some`
/// while it enters, stays or leaves. Draw the element whenever it is `Some`.
pub fn appear(id: impl Into<ElementId>, present: bool, window: &mut Window, cx: &mut App) -> Option<Appear> {
    let s = Presence::new(id.into(), present)
        .transition(Transition::new(scaled(APPEAR)).ease(cubic_bezier(0.32, 0.72, 0., 1.)))
        .sample(window, cx);
    // Presence counts its first 180 ms as a transition even when nothing was ever there: an
    // absent element that has not moved (t at 0) is gone, not leaving.
    (s.should_render() && (present || s.progress > 0.)).then_some(Appear { t: s.progress })
}

/// A width (or any length) on its way to `target`: 160 ms ease-out, the same curve. The value is
/// `target` at once under reduce motion.
pub fn slide(id: impl Into<ElementId>, target: Pixels, window: &mut Window, cx: &mut App) -> Pixels {
    gpui_base::transition(
        id.into(),
        target,
        Transition::new(scaled(SLIDE)).ease(cubic_bezier(0.32, 0.72, 0., 1.)),
        window,
        cx,
    )
}
