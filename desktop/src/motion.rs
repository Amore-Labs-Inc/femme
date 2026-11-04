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
        .transition(Transition::new(APPEAR).ease(cubic_bezier(0.32, 0.72, 0., 1.)))
        .sample(window, cx);
    s.should_render().then_some(Appear { t: s.progress })
}

/// A width (or any length) on its way to `target`: 160 ms ease-out, the same curve. The value is
/// `target` at once under reduce motion.
pub fn slide(id: impl Into<ElementId>, target: Pixels, window: &mut Window, cx: &mut App) -> Pixels {
    gpui_base::transition(
        id.into(),
        target,
        Transition::new(Duration::from_millis(160)).ease(cubic_bezier(0.32, 0.72, 0., 1.)),
        window,
        cx,
    )
}
