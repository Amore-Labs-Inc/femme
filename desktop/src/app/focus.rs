//! Whether a module the shell mounts has a focus handle of its own, decided at compile time.
//!
//! The modules below the shell are built by other agents and are not all [`Focusable`] (the contract
//! asks it of the composer only). The shell puts the keyboard in a module's own handle when its type
//! has one, and in its own region handle when not: autoref specialisation, picked at each
//! `focus_of!` call site, where the type is concrete. `(&Probe(x)).handle(cx)` takes `HasFocus` when
//! `T: Focusable` and `NoFocus` otherwise.

use gpui::{App, FocusHandle, Focusable};

/// A reference to a module, wrapped so a method call can choose between the two traits below.
// Dead until some module below implements `Focusable`: `HasFocus` is then never chosen.
#[allow(dead_code)]
pub(super) struct Probe<'a, T>(pub(super) &'a T);

/// Chosen when the module is [`Focusable`].
#[allow(dead_code)]
pub(super) trait HasFocus {
    /// The module's own focus handle.
    fn handle(&self, cx: &App) -> Option<FocusHandle>;
}

impl<T: Focusable> HasFocus for Probe<'_, T> {
    fn handle(&self, cx: &App) -> Option<FocusHandle> {
        Some(self.0.focus_handle(cx))
    }
}

/// Chosen otherwise (it needs one more auto-reference to match, so it loses to [`HasFocus`]).
pub(super) trait NoFocus {
    /// No handle of its own.
    fn handle(&self, cx: &App) -> Option<FocusHandle>;
}

impl<T> NoFocus for &Probe<'_, T> {
    fn handle(&self, _: &App) -> Option<FocusHandle> {
        None
    }
}

/// The focus handle of `$view` (a `&T`) when `T` is [`Focusable`], `None` when it is not.
macro_rules! focus_of {
    ($view:expr, $cx:expr) => {{
        #[allow(unused_imports)]
        use $crate::app::focus::{HasFocus as _, NoFocus as _};
        #[allow(clippy::needless_borrow)]
        let handle = (&$crate::app::focus::Probe($view)).handle($cx);
        handle
    }};
}
