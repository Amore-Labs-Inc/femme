//! The rows of the list. This first cut shows each row's key as a line; the builders for the kinds
//! of item follow.

use gpui::{div, AnyElement, Context, IntoElement as _, ParentElement as _, Window};

use super::live::Transcript;

impl Transcript {
    /// The element of row `ix` of the list.
    pub(super) fn row(&mut self, ix: usize, _: &mut Window, _: &mut Context<Self>) -> AnyElement {
        let label = self
            .layout
            .rows
            .get(ix)
            .map_or_else(String::new, |r| format!("{:?}", r.key));
        div().child(label).into_any_element()
    }
}
